// Toasts: a short note after an action ("Deleted Budget.xlsx"), with an optional action button
// (Undo) and a close button. They stack at the bottom-left on wide screens and across the bottom
// on phones, at most three at once (the oldest goes first), and each leaves after 8 seconds,
// waiting while the pointer or focus is on the stack. Screen readers hear them through a polite
// live region, failures through an assertive one; a toast never takes focus.
//
// The queue (createToastQueue) is pure, with an injectable clock, so it is tested without a page.

export const TOAST_LIMIT = 3;
export const TOAST_DURATION = 8000;

const realClock = {
  now: () => Date.now(),
  set: (fn, ms) => setTimeout(fn, ms),
  clear: (timer) => clearTimeout(timer),
};

/**
 * A queue of toasts. `add({ message, kind, action, duration })` answers the new toast's id;
 * `dismiss(id)` takes one away; `pause()` and `resume()` hold and restart every timer (the time
 * left is kept); `latestAction()` is the newest toast that still offers its action. `onChange`
 * gets the toasts shown, oldest first, after every change. `clock` is `{ now, set, clear }`.
 */
export function createToastQueue({
  limit = TOAST_LIMIT,
  duration = TOAST_DURATION,
  clock = realClock,
  onChange = () => {},
} = {}) {
  const toasts = [];
  let paused = false;
  let nextId = 1;

  const view = (toast) => ({ id: toast.id, message: toast.message, kind: toast.kind, action: toast.action });
  const changed = () => onChange(toasts.map(view));

  function start(toast) {
    if (paused || toast.timer !== null || !Number.isFinite(toast.remaining)) return;
    toast.started = clock.now();
    toast.timer = clock.set(() => {
      toast.timer = null;
      dismiss(toast.id);
    }, toast.remaining);
  }

  function stop(toast) {
    if (toast.timer === null) return;
    clock.clear(toast.timer);
    toast.timer = null;
    toast.remaining = Math.max(0, toast.remaining - (clock.now() - toast.started));
  }

  function add({ message, kind = 'info', action = null, duration: own } = {}) {
    const toast = {
      id: nextId++,
      message: String(message ?? ''),
      kind,
      action,
      remaining: own ?? duration,
      started: 0,
      timer: null,
    };
    toasts.push(toast);
    start(toast);
    while (toasts.length > limit) {
      const oldest = toasts.shift();
      stop(oldest);
    }
    changed();
    return toast.id;
  }

  function dismiss(id) {
    const at = toasts.findIndex((toast) => toast.id === id);
    if (at < 0) return false;
    const [toast] = toasts.splice(at, 1);
    stop(toast);
    changed();
    return true;
  }

  return {
    add,
    dismiss,
    pause() {
      if (paused) return;
      for (const toast of toasts) stop(toast);
      paused = true;
    },
    resume() {
      if (!paused) return;
      paused = false;
      for (const toast of toasts) start(toast);
    },
    get paused() {
      return paused;
    },
    list: () => toasts.map(view),
    /** The milliseconds left before `id` leaves (as of now), or null when it is not shown. */
    remaining(id) {
      const toast = toasts.find((t) => t.id === id);
      if (!toast) return null;
      return toast.timer === null ? toast.remaining : Math.max(0, toast.remaining - (clock.now() - toast.started));
    },
    latestAction() {
      for (let at = toasts.length - 1; at >= 0; at -= 1) if (toasts[at].action) return view(toasts[at]);
      return null;
    },
  };
}

// ---- the page part ----

let ui = null;

function el(tag, attributes = {}, ...children) {
  const element = document.createElement(tag);
  for (const [name, value] of Object.entries(attributes)) {
    if (value != null && value !== false) element.setAttribute(name, value === true ? '' : String(value));
  }
  element.append(...children.filter((child) => child != null));
  return element;
}

/** Builds the stack and the two live regions once, at the end of the page. */
function setup() {
  if (ui) return ui;
  const list = el('ol', { class: 'toast-list' });
  const stack = el('section', { class: 'toasts', 'aria-label': 'Notifications' }, list);
  const polite = el('p', { class: 'visually-hidden', role: 'status', 'aria-live': 'polite' });
  const assertive = el('p', { class: 'visually-hidden', role: 'alert', 'aria-live': 'assertive' });
  document.body.append(stack, polite, assertive);
  const shown = new Map(); // id -> { item, back }
  let hovered = false;
  const queue = createToastQueue({ onChange: (toasts) => draw(toasts) });
  ui = { list, stack, polite, assertive, shown, queue };

  stack.addEventListener('mouseenter', () => {
    hovered = true;
    queue.pause();
  });
  stack.addEventListener('mouseleave', () => {
    hovered = false;
    if (!stack.contains(document.activeElement)) queue.resume();
  });
  stack.addEventListener('focusin', () => queue.pause());
  stack.addEventListener('focusout', (event) => {
    if (!hovered && !stack.contains(event.relatedTarget)) queue.resume();
  });

  function draw(toasts) {
    const ids = new Set(toasts.map((toast) => toast.id));
    for (const [id, entry] of shown) {
      if (ids.has(id)) continue;
      const hadFocus = entry.item.contains(document.activeElement);
      entry.item.remove();
      shown.delete(id);
      if (hadFocus) {
        // Focus goes to another toast, or back where it was when this one came.
        const other = list.querySelector('.toast-close');
        const back = entry.back?.isConnected && !stack.contains(entry.back) ? entry.back : null;
        (other ?? back)?.focus();
      }
    }
    for (const toast of toasts) {
      if (shown.has(toast.id)) continue;
      const item = toastItem(toast);
      shown.set(toast.id, { item, back: null });
      list.append(item);
    }
    stack.hidden = toasts.length === 0;
    if (toasts.length === 0 && !hovered) queue.resume();
  }
  return ui;
}

function toastItem(toast) {
  const close = el('button', { type: 'button', class: 'toast-close plain', 'aria-label': `Close: ${toast.message}` }, '×');
  close.addEventListener('click', () => ui.queue.dismiss(toast.id));
  let action = null;
  if (toast.action) {
    action = el('button', { type: 'button', class: 'toast-action plain' }, toast.action.label);
    action.addEventListener('click', () => runAction(toast));
  }
  return el(
    'li',
    { class: `toast${toast.kind === 'error' ? ' error' : ''}`, 'data-id': toast.id },
    el('p', { class: 'toast-text' }, toast.message),
    action,
    close,
  );
}

/** Runs a toast's action once: the toast leaves first, so it cannot be used twice. */
function runAction(toast) {
  if (!ui.queue.dismiss(toast.id)) return;
  Promise.resolve()
    .then(() => toast.action.run())
    .catch(() => showToast('That did not work. Try again.', { kind: 'error' }));
}

/** Says `text` through the live region for its kind (cleared first, so a repeat is heard). */
function announce(text, kind) {
  const region = kind === 'error' ? ui.assertive : ui.polite;
  region.textContent = '';
  setTimeout(() => {
    region.textContent = text;
  }, 50);
}

/**
 * Shows a toast. `kind` is 'info' (the default), 'ok' or 'error' (announced assertively);
 * `action` is `{ label, run }` (an Undo); `announce` replaces what screen readers are told.
 * Answers the toast's id.
 */
export function showToast(message, { kind = 'info', action = null, announce: spoken } = {}) {
  if (typeof document === 'undefined') return null;
  setup();
  const back = document.activeElement;
  const id = ui.queue.add({ message, kind, action });
  // add() drew the toast; remember where focus was when it came.
  const entry = ui.shown.get(id);
  if (entry) entry.back = back;
  announce(spoken ?? message, kind);
  return id;
}

/** Takes the toast `id` away. */
export function dismissToast(id) {
  ui?.queue.dismiss(id);
}

/** Runs the newest action (Undo) still on screen; answers whether there was one. */
export function runLatestAction() {
  const toast = ui?.queue.latestAction();
  if (!toast) return false;
  runAction(toast);
  return true;
}
