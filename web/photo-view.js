// The Album view of an image library: its pictures edge to edge in justified rows that keep
// each one's shape, by day (newest first) or in the listing's order. Names and select boxes
// stay out of the way: a box shows on hover or focus, and once anything is selected every box
// shows and a click selects instead of opening. On a touch screen a long press selects.
//
// The grid takes one Tab stop; the arrow keys, Home, End, Page Up and Page Down move between
// pictures, Enter opens one in the viewer and Space selects it. Thumbnails (the large size)
// load as they come into view. Positions are set through CSSOM custom properties; the maths
// is in photos.js (tested).

import { h, seg, showThumbnail } from './common.js';
import { dayLabel, groupByDay, isPhoto, justify, moveIn, ratioOf, rowHeightFor, tileLabel } from './photos.js';

/** How long a press on a touch screen takes to select. */
const LONG_PRESS_MS = 500;

/**
 * Draws the Album view into `root` (its `.photo-groups` box) and answers
 * `{ show(items), setGrouped(on), refreshSelection(), focusPhoto(id), photos(), current() }`
 * (`current()` is the id of the picture that has, or last had, focus).
 * `isSelected(id)` and `toggle(id)` are the page's selection; `open(list, index)` opens the viewer.
 */
export function createPhotoView({ root, isSelected, toggle, open, language = undefined }) {
  const groupsBox = root.querySelector('.photo-groups');
  let all = []; // the loaded listing
  let photos = []; // the pictures in the order shown
  let grouped = true;
  let tiles = []; // { el, button, item, x, y, w, h, row } per picture, in order
  let groups = []; // { el, list, items }
  let focusAt = 0;
  let width = 0;
  let relayoutFrame = 0;
  const known = new Map(); // id → shape learned from a loaded thumbnail, for pictures with no facts
  let watcher = null;
  let press = null; // a touch press that may become a long press
  let pressed = false; // the click after a long press is not a click

  function show(items) {
    all = items;
    build();
  }

  function setGrouped(on) {
    grouped = on;
    build();
  }

  function build() {
    const focusedId = root.contains(document.activeElement) ? document.activeElement.closest?.('[data-id]')?.dataset.id : null;
    watcher?.disconnect();
    watcher = 'IntersectionObserver' in globalThis ? new IntersectionObserver(onSeen, { rootMargin: '400px' }) : null;
    const days = groupByDay(all.filter(isPhoto), grouped);
    photos = days.flatMap((day) => day.items);
    tiles = [];
    groups = days.map((day, g) => {
      const headingId = `photo-day-${g}`;
      const list = h('ul', { class: 'photo-grid', role: 'list', 'aria-labelledby': grouped ? headingId : 'items-heading' });
      for (const item of day.items) list.append(tile(item));
      const el = h(
        'section',
        { class: 'photo-day' },
        grouped ? h('h3', { class: 'photo-day-name', id: headingId }, dayLabel(day.day, new Date(), language)) : null,
        list,
      );
      return { el, list, items: day.items };
    });
    groupsBox.replaceChildren(...groups.map((g) => g.el));
    root.classList.toggle('grouped', grouped);
    width = 0;
    layout();
    const again = focusedId ? tiles.findIndex((t) => t.item.id === focusedId) : -1;
    focusAt = again >= 0 ? again : Math.min(focusAt, Math.max(0, tiles.length - 1));
    setTabStop();
    if (again >= 0) tiles[again].button.focus({ preventScroll: true });
    refreshSelection();
  }

  function tile(item) {
    const box = h('span', { class: 'photo-img', 'aria-hidden': 'true' });
    box.dataset.id = item.id;
    const button = h('button', { type: 'button', class: 'photo-open', tabindex: '-1', 'aria-label': tileLabel(item, language) }, box);
    const check = h('input', { type: 'checkbox', class: 'photo-check', tabindex: '-1', 'aria-label': `Select ${item.name}` });
    const el = h('li', { class: 'photo' }, button, check);
    el.dataset.id = item.id;
    const at = tiles.length;
    tiles.push({ el, button, check, item, box });
    button.addEventListener('click', () => {
      if (pressed) {
        pressed = false;
        return;
      }
      focusAt = at;
      setTabStop();
      if (root.classList.contains('selecting')) toggle(item.id);
      else open(photos, at);
    });
    button.addEventListener('focus', () => {
      focusAt = at;
      setTabStop();
    });
    check.addEventListener('change', () => toggle(item.id));
    check.addEventListener('click', (event) => event.stopPropagation());
    if (watcher) watcher.observe(box);
    else load(box);
    return el;
  }

  function onSeen(entries) {
    for (const entry of entries) {
      if (!entry.isIntersecting) continue;
      watcher?.unobserve(entry.target);
      load(entry.target);
    }
  }

  /** Loads a tile's thumbnail; a picture with no facts takes the shape of what loads. */
  function load(box) {
    const id = box.dataset.id;
    const img = showThumbnail(box, `/api/docs/${seg(id)}/thumbnail?size=large`);
    img.decoding = 'async';
    const item = photos.find((p) => p.id === id);
    if (item && !(item.photo?.width > 0)) {
      img.addEventListener(
        'load',
        () => {
          if (img.naturalWidth > 0 && img.naturalHeight > 0 && !known.has(id)) {
            known.set(id, img.naturalWidth / img.naturalHeight);
            scheduleLayout();
          }
        },
        { once: true },
      );
    }
  }

  function scheduleLayout() {
    if (relayoutFrame) return;
    relayoutFrame = requestAnimationFrame(() => {
      relayoutFrame = 0;
      width = 0;
      layout();
    });
  }

  /** Places every tile for the width there is now; nothing is drawn again when it has not changed. */
  function layout() {
    const now = Math.floor(groupsBox.clientWidth);
    if (now === width || now === 0) return;
    width = now;
    const height = rowHeightFor(width);
    let at = 0;
    let rowBase = 0;
    for (const group of groups) {
      const placed = justify(
        group.items.map((item) => ratioOf(item, known)),
        width,
        { height },
      );
      group.list.style.setProperty('--grid-h', `${placed.height}px`);
      for (const spot of placed.tiles) {
        const t = tiles[at];
        Object.assign(t, { x: spot.x, y: spot.y, w: spot.w, h: spot.h, row: rowBase + spot.row });
        t.el.style.setProperty('--x', `${spot.x}px`);
        t.el.style.setProperty('--y', `${spot.y}px`);
        t.el.style.setProperty('--w', `${spot.w}px`);
        t.el.style.setProperty('--h', `${spot.h}px`);
        at += 1;
      }
      rowBase += placed.rows.length;
    }
  }

  function setTabStop() {
    for (const [i, t] of tiles.entries()) t.button.tabIndex = i === focusAt ? 0 : -1;
  }

  /** The boxes and marks follow the page's selection; with any selected, every box shows. */
  function refreshSelection() {
    let any = false;
    for (const t of tiles) {
      const on = isSelected(t.item.id);
      any ||= on;
      t.check.checked = on;
      t.el.classList.toggle('selected', on);
      t.button.setAttribute('aria-pressed', String(on));
    }
    root.classList.toggle('selecting', any);
  }

  function focusPhoto(id) {
    const at = tiles.findIndex((t) => t.item.id === id);
    if (at < 0) return;
    focusAt = at;
    setTabStop();
    tiles[at].button.focus({ preventScroll: true });
    tiles[at].el.scrollIntoView?.({ block: 'nearest' });
  }

  groupsBox.addEventListener('keydown', (event) => {
    if (event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return;
    const at = tiles.findIndex((t) => t.button === event.target);
    if (at < 0) return;
    if (event.key === ' ') {
      event.preventDefault();
      toggle(tiles[at].item.id);
      return;
    }
    const page = Math.max(1, Math.floor(innerHeight / (rowHeightFor(width) + 4)) - 1);
    const to = moveIn(tiles, at, event.key, page);
    if (to === null) return;
    event.preventDefault();
    focusPhoto(tiles[to].item.id);
  });

  // A long press on a touch screen selects; moving the finger first is a scroll.
  groupsBox.addEventListener('pointerdown', (event) => {
    if (event.pointerType !== 'touch') return;
    const t = tiles.find((x) => x.button === event.target.closest?.('.photo-open'));
    if (!t) return;
    const start = { x: event.clientX, y: event.clientY };
    press = {
      start,
      timer: setTimeout(() => {
        press = null;
        pressed = true;
        toggle(t.item.id);
        navigator.vibrate?.(10);
      }, LONG_PRESS_MS),
    };
  });
  const cancelPress = () => {
    if (press) clearTimeout(press.timer);
    press = null;
  };
  groupsBox.addEventListener('pointermove', (event) => {
    if (press && Math.hypot(event.clientX - press.start.x, event.clientY - press.start.y) > 10) cancelPress();
  });
  groupsBox.addEventListener('pointerup', cancelPress);
  groupsBox.addEventListener('pointercancel', cancelPress);
  // The long press's own menu (Save image and so on) would cover the selection.
  groupsBox.addEventListener('contextmenu', (event) => {
    if (pressed || press) event.preventDefault();
  });

  if ('ResizeObserver' in globalThis) new ResizeObserver(() => layout()).observe(groupsBox);
  else addEventListener('resize', () => layout());

  return { show, setGrouped, refreshSelection, focusPhoto, photos: () => photos, current: () => tiles[focusAt]?.item.id ?? null };
}
