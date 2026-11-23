// The full-screen viewer of the Album view, and its slideshow: one picture at a time fitted to
// the window on a dark stage, with Previous and Next (← and →, the edge buttons, a swipe), its
// name, when it was taken and a download; Escape or a swipe down closes it. The slideshow steps
// on by itself every few seconds (chosen), pauses and plays (Space), and can go round again.
//
// A picture shows its large thumbnail at once and then the whole file from its preview route
// when that has loaded; only the picture shown and the next one are loaded, never the rest.
// The maths is in photos.js (tested).

import { h, seg } from './common.js';
import { DEFAULT_INTERVAL, INTERVALS, positionOf, stepIndex, swipeOf, viewerDate } from './photos.js';

/** How long the controls stay after the pointer last moved, while a slideshow plays. */
const IDLE_MS = 2500;

const thumbnailUrl = (item) => `/api/docs/${seg(item.id)}/thumbnail?size=large`;
const previewUrl = (item) => `/api/docs/${seg(item.id)}/preview`;
const downloadUrl = (item) => `/api/docs/${seg(item.id)}/content`;

/**
 * Makes the viewer (a modal dialog added to the page) and answers `{ open, close, isOpen }`.
 * `open(list, index, { play })` shows `list[index]`, starting the slideshow when `play`;
 * `onClose(item)` is told which picture was shown last, so the page can focus it.
 */
export function createPhotoViewer({ onClose = () => {}, language = undefined } = {}) {
  let list = [];
  let index = 0;
  let playing = false;
  let loop = true;
  let interval = DEFAULT_INTERVAL;
  let timer = 0;
  let idleTimer = 0;
  let shownFor = null; // the item whose picture is on the stage
  const loaded = new Map(); // id → <img> of the whole file, loaded or loading (the current and next)

  // Focus rests here, so the keys (Space above all) reach the viewer rather than a button.
  const stage = h('div', { class: 'ph-stage', tabindex: '-1' });
  const title = h('h2', { class: 'ph-name', id: 'ph-name' });
  const when = h('p', { class: 'ph-when' });
  const where = h('p', { class: 'ph-where' });
  const closeButton = h('button', { type: 'button', class: 'ph-close ph-icon', 'aria-label': 'Close', title: 'Close (Esc)' }, '✕');
  const previous = h('button', { type: 'button', class: 'ph-step ph-previous', 'aria-label': 'Previous photo', title: 'Previous (←)' }, h('span', { 'aria-hidden': 'true' }, '‹'));
  const next = h('button', { type: 'button', class: 'ph-step ph-next', 'aria-label': 'Next photo', title: 'Next (→)' }, h('span', { 'aria-hidden': 'true' }, '›'));
  const play = h('button', { type: 'button', class: 'ph-play', 'aria-pressed': 'false' }, 'Slideshow');
  const speed = h(
    'select',
    { class: 'ph-speed', 'aria-label': 'Seconds per photo' },
    ...INTERVALS.map((s) => h('option', { value: String(s), selected: s === DEFAULT_INTERVAL }, `${s} s`)),
  );
  const loopBox = h('input', { type: 'checkbox', class: 'ph-loop', checked: true });
  const loopLabel = h('label', { class: 'ph-loop-label' }, loopBox, ' Loop');
  const download = h('a', { class: 'button plain ph-download', download: '' }, 'Download');
  const announcer = h('p', { class: 'visually-hidden', 'aria-live': 'polite' });
  const top = h('div', { class: 'ph-top' }, h('div', { class: 'ph-title' }, title, when, where), closeButton);
  const bar = h('div', { class: 'ph-bar', role: 'toolbar', 'aria-label': 'Photo' }, play, speed, loopLabel, download);
  const dialog = h('dialog', { class: 'photo-viewer', 'aria-labelledby': 'ph-name' }, stage, previous, next, top, bar, announcer);
  document.body.append(dialog);

  function current() {
    return list[index] ?? null;
  }

  /** The whole file of `item` as an image element, made (and so loaded) once. */
  function full(item) {
    let img = loaded.get(item.id);
    if (!img) {
      img = new Image();
      img.decoding = 'async';
      img.src = previewUrl(item);
      loaded.set(item.id, img);
    }
    return img;
  }

  /** Keeps only the current picture's file and the next one's loaded. */
  function trim() {
    const keep = new Set([current()?.id, list[stepIndex(index, list.length, 1, loop)]?.id]);
    for (const id of loaded.keys()) if (!keep.has(id)) loaded.delete(id);
  }

  function show() {
    const item = current();
    if (!item) return;
    shownFor = item;
    clearTimeout(timer);
    title.textContent = item.name;
    when.textContent = viewerDate(item, language);
    where.textContent = positionOf(index, list.length);
    download.href = downloadUrl(item);
    download.setAttribute('aria-label', `Download ${item.name}`);
    previous.disabled = stepIndex(index, list.length, -1, playing && loop) === null;
    next.disabled = stepIndex(index, list.length, 1, playing && loop) === null;
    // Focus on a button that has just turned off goes to the other one, not out of the dialog.
    if (document.activeElement === next && next.disabled) previous.focus();
    else if (document.activeElement === previous && previous.disabled) next.focus();
    announcer.textContent = `${item.name}, ${positionOf(index, list.length)}`;
    // The large thumbnail first (shown at once when the grid loaded it), then the whole file,
    // the very element that loaded it (its route is never cached, so a copy would load again).
    const whole = full(item);
    whole.className = 'ph-image ph-full';
    whole.alt = item.name;
    const swap = () => {
      if (shownFor !== item) return;
      stage.replaceChildren(whole);
      scheduleNext();
    };
    if (!(whole.complete && whole.naturalWidth > 0)) {
      stage.replaceChildren(h('img', { class: 'ph-image', alt: item.name, src: thumbnailUrl(item) }));
    }
    const failed = () => {
      if (shownFor !== item) return;
      // The thumbnail stays; the slideshow goes on.
      scheduleNext();
    };
    if (whole.complete && whole.naturalWidth > 0) swap();
    else if (whole.complete && whole.src) failed();
    else {
      whole.addEventListener('load', swap, { once: true });
      whole.addEventListener('error', failed, { once: true });
    }
    // The next one loads meanwhile.
    const after = list[stepIndex(index, list.length, 1, loop)];
    if (after && after !== item) full(after);
    trim();
  }

  function scheduleNext() {
    clearTimeout(timer);
    if (!playing || !dialog.open) return;
    timer = setTimeout(() => {
      const to = stepIndex(index, list.length, 1, loop);
      if (to === null) {
        setPlaying(false);
        announcer.textContent = 'End of the slideshow';
        return;
      }
      index = to;
      show();
    }, interval * 1000);
  }

  function step(by) {
    const to = stepIndex(index, list.length, by, playing && loop);
    if (to === null) return;
    index = to;
    show();
  }

  function setPlaying(on) {
    playing = on;
    play.setAttribute('aria-pressed', String(on));
    play.textContent = on ? 'Pause' : 'Slideshow';
    play.title = on ? 'Pause the slideshow (Space)' : 'Play a slideshow (Space)';
    dialog.classList.toggle('playing', on);
    if (!on) {
      clearTimeout(timer);
      wake();
    } else {
      wake();
      scheduleNext();
    }
    previous.disabled = stepIndex(index, list.length, -1, on && loop) === null;
    next.disabled = stepIndex(index, list.length, 1, on && loop) === null;
  }

  /** Shows the controls again; while playing they fade after a while without the pointer moving. */
  function wake() {
    dialog.classList.remove('idle');
    clearTimeout(idleTimer);
    if (playing) idleTimer = setTimeout(() => dialog.classList.add('idle'), IDLE_MS);
  }

  function open(items, at = 0, { play: start = false } = {}) {
    if (!items.length) return;
    list = items;
    index = Math.min(Math.max(at, 0), items.length - 1);
    if (!dialog.open) dialog.showModal();
    setPlaying(false);
    show();
    if (start) {
      setPlaying(true);
      // Full screen for a slideshow, where the browser allows it; the dialog fills the window anyway.
      dialog.requestFullscreen?.().catch(() => {});
    }
    (start ? play : stage).focus();
  }

  function close() {
    if (dialog.open) dialog.close();
  }

  dialog.addEventListener('close', () => {
    const last = current();
    setPlaying(false);
    clearTimeout(idleTimer);
    loaded.clear();
    shownFor = null;
    stage.replaceChildren();
    if (document.fullscreenElement === dialog) document.exitFullscreen?.().catch(() => {});
    onClose(last);
  });
  closeButton.addEventListener('click', close);
  previous.addEventListener('click', () => step(-1));
  next.addEventListener('click', () => step(1));
  play.addEventListener('click', () => setPlaying(!playing));
  speed.addEventListener('change', () => {
    interval = Number(speed.value) || DEFAULT_INTERVAL;
    scheduleNext();
  });
  loopBox.addEventListener('change', () => {
    loop = loopBox.checked;
    setPlaying(playing);
    trim();
  });
  dialog.addEventListener('keydown', (event) => {
    if (event.altKey || event.ctrlKey || event.metaKey) return;
    const onControl = event.target.closest?.('select, input, a, button:not(.ph-step)');
    switch (event.key) {
      case 'ArrowLeft':
        if (event.target === speed) return;
        step(-1);
        break;
      case 'ArrowRight':
        if (event.target === speed) return;
        step(1);
        break;
      case 'Home':
        index = 0;
        show();
        break;
      case 'End':
        index = list.length - 1;
        show();
        break;
      case ' ':
        if (onControl) return;
        setPlaying(!playing);
        break;
      default:
        wake();
        return;
    }
    wake();
    event.preventDefault();
  });
  dialog.addEventListener('pointermove', wake);
  // A click on the dark stage around the picture closes, as on most viewers.
  stage.addEventListener('click', (event) => {
    if (event.target === stage) close();
  });

  // Swipes on a touch screen: left and right step, down closes.
  let touch = null;
  stage.addEventListener('pointerdown', (event) => {
    if (event.pointerType !== 'touch' || !event.isPrimary) return;
    touch = { x: event.clientX, y: event.clientY, t: event.timeStamp };
  });
  stage.addEventListener('pointerup', (event) => {
    if (!touch || event.pointerType !== 'touch') return;
    const what = swipeOf(event.clientX - touch.x, event.clientY - touch.y, event.timeStamp - touch.t);
    touch = null;
    if (what === 'next') step(1);
    else if (what === 'previous') step(-1);
    else if (what === 'close') close();
    else wake();
  });
  stage.addEventListener('pointercancel', () => {
    touch = null;
  });
  // A slideshow in a hidden tab waits.
  document.addEventListener('visibilitychange', () => {
    if (document.hidden) clearTimeout(timer);
    else scheduleNext();
  });

  return { open, close, isOpen: () => dialog.open };
}
