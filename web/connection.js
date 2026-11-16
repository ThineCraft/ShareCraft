// The connection banner (round 25): when ShareCraft cannot be reached, one slim banner says so
// at the top of the page, counts down to the next try, offers Retry now, and goes with a "Back
// online" toast once the server answers again. `api` in common.js reports to it: every request
// that fails with a network error, and every answer. Pages never show their own network lines.
//
// While the browser says it is offline, requests wait for the connection (a change is sent once
// it is back: that is what the banner promises); a read that failed because the server could not
// be reached is tried again once it can be. A change whose request was cut off in the middle is
// never sent twice: the page is told, as before.

/** Seconds between tries while the server cannot be reached: 5, 10, 20, 30, then every 60. */
export const RETRY_STEPS = Object.freeze([5, 10, 20, 30, 60]);

/** Milliseconds before try number `attempt` (0 for the first). */
export function retryDelay(attempt) {
  const step = RETRY_STEPS[Math.min(Math.max(0, Math.floor(attempt) || 0), RETRY_STEPS.length - 1)];
  return step * 1000;
}

/** The first `count` delays, in milliseconds (for the tests and the note). */
export const retrySchedule = (count) => Array.from({ length: count }, (_, attempt) => retryDelay(attempt));

/**
 * What the banner says in state `state` ('online', 'offline', 'unreachable' or 'trying') with
 * `seconds` left before the next try: `{ text, spoken, retry }`, or null when there is nothing
 * to say. `spoken` is said once when the state starts (the countdown is not read out).
 */
export function bannerModel(state, seconds = 0) {
  if (state === 'offline') {
    const text = 'You’re offline — changes will be saved when you’re back';
    return { text, spoken: 'You’re offline. Changes will be saved when you’re back.', retry: false };
  }
  if (state === 'unreachable' || state === 'trying') {
    const text =
      state === 'trying' || seconds <= 0
        ? 'Can’t reach ShareCraft — trying again…'
        : `Can’t reach ShareCraft — retrying in ${seconds} s`;
    return { text, spoken: 'Can’t reach ShareCraft. Trying again automatically.', retry: state !== 'trying' };
  }
  return null;
}

/**
 * The connection's state machine, without the page: `probe()` answers true when the server
 * answered (any status) and false when it could not be reached; `setTimer(fn, ms)` and
 * `clearTimer(id)` schedule the next try; `onChange(state, info)` is told every change, with
 * `{ back: true }` when the connection came back and `{ due }` (the time of the next try) while
 * waiting. `now` is the clock.
 */
export function createConnection({ probe, setTimer = setTimeout, clearTimer = clearTimeout, now = () => Date.now(), onChange = () => {} }) {
  let state = 'online';
  let attempt = 0;
  let timer = null;
  let due = 0;
  let waiters = [];

  const set = (next, info = {}) => {
    state = next;
    onChange(state, info);
  };
  const schedule = () => {
    if (timer !== null) clearTimer(timer);
    const wait = retryDelay(attempt);
    due = now() + wait;
    timer = setTimer(() => {
      timer = null;
      tryNow();
    }, wait);
    set('unreachable', { due });
  };
  const back = () => {
    if (timer !== null) clearTimer(timer);
    timer = null;
    attempt = 0;
    const was = state;
    const resolve = waiters;
    waiters = [];
    if (was !== 'online') set('online', { back: true });
    for (const done of resolve) done(true);
  };
  const tryNow = async () => {
    if (state === 'offline' || state === 'trying') return;
    if (timer !== null) clearTimer(timer);
    timer = null;
    set('trying');
    let reached = false;
    try {
      reached = await probe();
    } catch {
      reached = false;
    }
    if (state === 'offline') return; // the browser went offline meanwhile; "online" restarts it
    if (reached) back();
    else {
      attempt += 1;
      schedule();
    }
  };

  return {
    get state() {
      return state;
    },
    get due() {
      return due;
    },
    get attempt() {
      return attempt;
    },
    /** A request could not reach the server. */
    lost() {
      if (state !== 'online') return;
      attempt = 0;
      schedule();
    },
    /** A request was answered: the server can be reached. */
    reached() {
      if (state !== 'online') back();
    },
    /** The browser says it is offline. */
    offline() {
      if (timer !== null) clearTimer(timer);
      timer = null;
      set('offline');
    },
    /** The browser says it is online again: try at once. */
    online() {
      if (state !== 'offline') return;
      state = 'unreachable';
      attempt = 0;
      tryNow();
    },
    /** Retry now. */
    retry: () => tryNow(),
    /**
     * Resolves true when the connection is back, or false after `limit` milliseconds; at once
     * (true) when nothing is wrong.
     */
    whenBack(limit = 120_000) {
      if (state === 'online') return Promise.resolve(true);
      return new Promise((resolve) => {
        let settled = false;
        const finish = (value) => {
          if (settled) return;
          settled = true;
          resolve(value);
        };
        waiters.push(finish);
        setTimer(() => finish(false), limit);
      });
    },
  };
}

// ---- in the browser: one connection per page, its banner and its toast ----

let shared = null;

/** The page's connection (null outside a browser). Its banner is drawn the first time it is needed. */
export function connection() {
  if (shared) return shared;
  if (typeof document === 'undefined' || typeof window === 'undefined') return null;
  let banner = null;
  let tick = null;
  const draw = (state, info) => {
    // The editor and viewer pages say how their connection is in their own bar.
    if (document.querySelector('.frame')) return;
    clearInterval(tick);
    tick = null;
    if (info.back) {
      if (banner) banner.hidden = true;
      import('./toast.js').then((toast) => toast.showToast('Back online', { kind: 'ok' })).catch(() => {});
      return;
    }
    const left = () => Math.max(0, Math.ceil((shared.due - Date.now()) / 1000));
    const model = bannerModel(state, left());
    if (!model) {
      if (banner) banner.hidden = true;
      return;
    }
    const fresh = !banner || banner.hidden || banner.dataset.state !== (state === 'trying' ? 'unreachable' : state);
    banner = banner ?? buildBanner(() => shared.retry());
    banner.hidden = false;
    banner.dataset.state = state === 'trying' ? 'unreachable' : state;
    banner.querySelector('.conn-text').textContent = model.text;
    banner.querySelector('.conn-retry').hidden = !model.retry;
    if (fresh) {
      // Said once, a moment after the live region is in place; the countdown is not read out.
      const live = banner.querySelector('.conn-live');
      live.textContent = '';
      setTimeout(() => {
        live.textContent = model.spoken;
      }, 120);
    }
    if (state === 'unreachable') {
      tick = setInterval(() => {
        const next = bannerModel('unreachable', left());
        if (next && banner) banner.querySelector('.conn-text').textContent = next.text;
      }, 1000);
    }
  };
  shared = createConnection({
    probe: async () => {
      try {
        await fetch('/api/me', { credentials: 'same-origin', cache: 'no-store' });
        return true;
      } catch {
        return false;
      }
    },
    onChange: draw,
  });
  addEventListener('offline', () => shared.offline());
  addEventListener('online', () => shared.online());
  if (globalThis.navigator?.onLine === false) shared.offline();
  return shared;
}

/** The banner: the sentence, a live line for screen readers and Retry now. */
function buildBanner(onRetry) {
  const text = document.createElement('span');
  text.className = 'conn-text';
  text.setAttribute('aria-hidden', 'true');
  const live = document.createElement('span');
  live.className = 'conn-live visually-hidden';
  live.setAttribute('role', 'status');
  const retry = document.createElement('button');
  retry.type = 'button';
  retry.className = 'plain small conn-retry';
  retry.textContent = 'Retry now';
  retry.addEventListener('click', onRetry);
  const banner = document.createElement('div');
  banner.className = 'conn-banner';
  banner.append(text, live, retry);
  document.body.append(banner);
  return banner;
}
