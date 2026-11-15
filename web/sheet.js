// The phone-first shell's pure parts (round 20), with no imports so that menu.js and shell.js can
// both use them: when the shell is on (`shellMode`), when the on-screen keyboard is up
// (`keyboardUp`), the long-press recogniser (`createLongPress`, pointer positions and times in,
// decisions out: the clock is the caller's) and a bottom sheet's state (`sheetReducer`, open,
// dragged and let go). `phoneShellNow()` and `dragToClose()` are the small page parts.

/** Below this width every pointer gets the phone shell. */
export const PHONE_BELOW = 700;
/** Below this width a coarse pointer (a finger) gets it too: tablets held upright, big phones. */
export const COARSE_BELOW = 900;

/**
 * 'phone' or 'desktop' for a viewport `width` pixels wide whose main pointer is `pointer`
 * ('coarse' for a finger, 'fine' for a mouse or pen, 'none'). A width that is not a number is
 * taken as desktop.
 */
export function shellMode(width, pointer = 'fine') {
  const w = Number(width);
  if (!Number.isFinite(w) || w <= 0) return 'desktop';
  if (w < PHONE_BELOW) return 'phone';
  if (pointer === 'coarse' && w < COARSE_BELOW) return 'phone';
  return 'desktop';
}

/** Whether this page is in the phone shell's size now (menus open as sheets then). */
export function phoneShellNow(win = globalThis) {
  if (!win?.matchMedia) return false;
  const coarse = win.matchMedia('(pointer: coarse)').matches ? 'coarse' : 'fine';
  return shellMode(win.innerWidth, coarse) === 'phone';
}

/**
 * Whether the on-screen keyboard is up: the visual viewport (`visible` pixels high) is much
 * shorter than the layout viewport (`layout`). 150 pixels or a quarter of the screen, whichever
 * is less, tells a keyboard from the browser's own bars coming and going.
 */
export function keyboardUp(layout, visible) {
  const full = Number(layout);
  const seen = Number(visible);
  if (!Number.isFinite(full) || !Number.isFinite(seen) || full <= 0) return false;
  return full - seen > Math.min(150, full / 4);
}

// ---- long press ----

/** How long a finger must rest, in milliseconds. */
export const LONG_PRESS_MS = 500;
/** How far it may wander, in pixels, and still be a press (not a scroll). */
export const LONG_PRESS_SLOP = 10;

/**
 * A long-press recogniser for one pointer at a time. Feed it the pointer's events with the time
 * they happened (`at`, milliseconds from any clock); each call answers what happened:
 *
 * - `down(point, at)`: 'waiting' (a press may be starting) or null (ignored: another pointer is
 *   down, or `point.button` is not the main one).
 * - `move(point, at)`: 'waiting', 'cancelled' (moved further than the slop: a scroll or a drag),
 *   'pressed' (time was up before it moved) or null.
 * - `tick(at)`: 'pressed' once the delay has passed (the caller's timer calls this), else null.
 * - `up(point, at)`: 'tap' (let go early: an ordinary tap, the click goes ahead), 'pressed' (let
 *   go after the delay although no tick came), 'after-press' (let go after a press: the click
 *   that follows must be swallowed) or null.
 * - `cancel()`: 'cancelled' (the browser took the pointer for scrolling, or the page scrolled).
 *
 * `state()` is 'idle', 'waiting' or 'pressed'.
 */
export function createLongPress({ delay = LONG_PRESS_MS, slop = LONG_PRESS_SLOP } = {}) {
  let phase = 'idle';
  let start = null; // { x, y, at, id }

  const reset = () => {
    phase = 'idle';
    start = null;
  };
  const same = (point) => !start || point?.pointerId === undefined || point.pointerId === start.id;

  return {
    state: () => phase,
    down(point, at) {
      if (phase !== 'idle') return null;
      if (point?.button !== undefined && point.button !== 0) return null;
      start = { x: Number(point?.x) || 0, y: Number(point?.y) || 0, at: Number(at), id: point?.pointerId };
      phase = 'waiting';
      return 'waiting';
    },
    move(point, at) {
      if (phase === 'idle' || !same(point)) return null;
      if (phase === 'pressed') return null;
      const dx = (Number(point?.x) || 0) - start.x;
      const dy = (Number(point?.y) || 0) - start.y;
      if (Math.hypot(dx, dy) > slop) {
        reset();
        return 'cancelled';
      }
      if (Number(at) - start.at >= delay) {
        phase = 'pressed';
        return 'pressed';
      }
      return 'waiting';
    },
    tick(at) {
      if (phase !== 'waiting') return null;
      if (Number(at) - start.at < delay) return null;
      phase = 'pressed';
      return 'pressed';
    },
    up(point, at) {
      if (phase === 'idle' || !same(point)) return null;
      const was = phase;
      const held = Number(at) - start.at;
      reset();
      if (was === 'pressed') return 'after-press';
      return held >= delay ? 'pressed' : 'tap';
    },
    cancel() {
      if (phase === 'idle') return null;
      const was = phase;
      reset();
      return was === 'pressed' ? null : 'cancelled';
    },
  };
}

// ---- bottom sheets ----

/** A sheet dragged down further than this many pixels closes when let go... */
export const SHEET_CLOSE_DISTANCE = 96;
/** ...as does one flicked down faster than this (pixels per millisecond). */
export const SHEET_CLOSE_SPEED = 0.6;

/** A closed sheet. */
export const SHEET_CLOSED = Object.freeze({ phase: 'closed', offset: 0, from: 0, at: 0 });

/**
 * The next state of a bottom sheet for `event`. States are `{ phase, offset, from, at }`:
 * phase 'closed', 'open' or 'dragging'; `offset` is how far it is pulled down (never up).
 * Events: {type:'open'}, {type:'close'} (Escape, a tap outside, a choice),
 * {type:'drag-start', y, at}, {type:'drag-move', y, at}, {type:'drag-end', y, at}. A drag let go
 * past SHEET_CLOSE_DISTANCE, or flicked down faster than SHEET_CLOSE_SPEED, closes; any other
 * goes back up.
 */
export function sheetReducer(state = SHEET_CLOSED, event = {}) {
  const now = state ?? SHEET_CLOSED;
  switch (event.type) {
    case 'open':
      return { phase: 'open', offset: 0, from: 0, at: 0 };
    case 'close':
      return SHEET_CLOSED;
    case 'drag-start':
      if (now.phase !== 'open') return now;
      return { phase: 'dragging', offset: 0, from: Number(event.y) || 0, at: Number(event.at) || 0, lastY: Number(event.y) || 0, lastAt: Number(event.at) || 0 };
    case 'drag-move': {
      if (now.phase !== 'dragging') return now;
      const y = Number(event.y) || 0;
      return { ...now, offset: Math.max(0, y - now.from), lastY: y, lastAt: Number(event.at) || now.lastAt, prevY: now.lastY, prevAt: now.lastAt };
    }
    case 'drag-end': {
      if (now.phase !== 'dragging') return now;
      const y = Number(event.y) || 0;
      const at = Number(event.at) || 0;
      const offset = Math.max(0, y - now.from);
      // speed over the last stretch of the drag (or the whole of it when that is all there is)
      const fromY = now.prevY ?? now.from;
      const fromAt = now.prevAt ?? now.at;
      const speed = at > fromAt ? (y - fromY) / (at - fromAt) : 0;
      if (offset > SHEET_CLOSE_DISTANCE || (offset > 16 && speed > SHEET_CLOSE_SPEED)) return SHEET_CLOSED;
      return { phase: 'open', offset: 0, from: 0, at: 0 };
    }
    default:
      return now;
  }
}

/**
 * Lets `sheet` (an element shown as a bottom sheet) be pulled down to close: a drag that starts
 * on `handle`, or anywhere on the sheet while its content is scrolled to the top and has nothing
 * to scroll, follows the finger (CSS custom property --sheet-drag) and closes it through
 * `onClose()` when let go far or fast enough. Returns a function that removes the listeners.
 */
export function dragToClose(sheet, onClose, { handle = null, now = () => performance.now() } = {}) {
  let state = sheetReducer(undefined, { type: 'open' });
  let pointer = null;
  let quietUntil = 0;
  const set = (offset) => {
    if (offset > 0) sheet.style.setProperty('--sheet-drag', `${offset}px`);
    else sheet.style.removeProperty('--sheet-drag');
    sheet.classList.toggle('sheet-dragging', state.phase === 'dragging');
  };
  const down = (event) => {
    if (event.pointerType === 'mouse' || pointer !== null) return;
    const onHandle = handle && handle.contains(event.target);
    const still = sheet.scrollTop <= 0 && sheet.scrollHeight <= sheet.clientHeight + 1;
    if (!onHandle && !still) return;
    pointer = event.pointerId;
    state = sheetReducer(state, { type: 'drag-start', y: event.clientY, at: now() });
  };
  const move = (event) => {
    if (event.pointerId !== pointer) return;
    state = sheetReducer(state, { type: 'drag-move', y: event.clientY, at: now() });
    set(state.offset);
  };
  const up = (event) => {
    if (event.pointerId !== pointer) return;
    pointer = null;
    const moved = state.offset;
    state = sheetReducer(state, { type: 'drag-end', y: event.clientY, at: now() });
    set(0);
    // the click that may end a drag is not a choice
    if (moved > 8) quietUntil = now() + 400;
    if (state.phase === 'closed') {
      onClose();
      state = sheetReducer(undefined, { type: 'open' });
    }
  };
  const swallow = (event) => {
    if (now() < quietUntil) {
      event.stopPropagation();
      event.preventDefault();
    }
  };
  const cancel = (event) => {
    if (event.pointerId !== pointer) return;
    pointer = null;
    state = sheetReducer(undefined, { type: 'open' });
    set(0);
  };
  sheet.addEventListener('pointerdown', down);
  sheet.addEventListener('pointermove', move);
  sheet.addEventListener('pointerup', up);
  sheet.addEventListener('pointercancel', cancel);
  sheet.addEventListener('click', swallow, true);
  return () => {
    sheet.removeEventListener('pointerdown', down);
    sheet.removeEventListener('pointermove', move);
    sheet.removeEventListener('pointerup', up);
    sheet.removeEventListener('pointercancel', cancel);
    sheet.removeEventListener('click', swallow, true);
    pointer = null;
    set(0);
  };
}
