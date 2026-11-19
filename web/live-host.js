// The frame page's side of a live session (G5): the page holds the one WebSocket and relays
// frames between it and the editor in the frame, which runs the live client. The page never
// reads a change or a presence note; it peeks only at the few server messages it acts on
// itself, waits longer after each failure, fetches the base the editor asks for, and uploads
// the editor's checkpoints and verifying hashes.
//
// The messages with the editor are `<prefix>:live-*` through postMessage between same-origin
// windows; their shapes match GridCraft's apps/gridcraft-web/src/live.rs.

/** The most bytes of one frame the editor may ask the page to send (the server's limit). */
export const MAX_SEND_BYTES = 1024 * 1024;
/** How often the editor is ticked, in milliseconds. */
export const TICK_MS = 60_000;
/** How long a shown editor has to answer a tick before it counts as stopped. */
export const WATCHDOG_MS = 15_000;
/** How many times a stopped editor is reloaded without asking. */
export const AUTO_RELOADS = 3;
/** Identical errors in a row after which the page stops reconnecting. */
export const MAX_REPEATS = 5;

const FIRST_WAIT = 1000;
const MAX_WAIT = 60_000;
const SPREAD = 0.2;
const MAX_KIND_CHARS = 64;
const MAX_TICKET_CHARS = 64;
const MAX_REVISION_CHARS = 128;
const MAX_PHASE_CHARS = 32;
const MAX_BASE_BYTES = 512 * 1024 * 1024;
const MAX_SESSION_DATA_BYTES = 8 * 1024 * 1024;
const MAX_PEOPLE = 256;
const SHA256 = /^[0-9a-f]{64}$/;

/** The server messages the page reads itself. Entries and presence notes are never among them. */
const PEEKED = new Set([
  'people',
  'access',
  'removed',
  'expired',
  'incompatible',
  'build_refused',
  'cannot_open',
  'changes_paused',
  'changes_resumed',
  'returning',
  'saved',
]);
/** Server messages after which the page stops for good. */
const FINAL = new Set(['incompatible', 'build_refused', 'cannot_open']);

/**
 * The wait before the next connection after `event`, as `{ wait, attempt, stopped, lastError,
 * repeats }`. Events: `{type: 'closed'}` (the socket closed), `{type: 'reset', kind}` (the
 * editor could not follow; `kind` names the error), `{type: 'live'}` (the session is live
 * again, which starts everything afresh). The wait starts at one second and doubles up to a
 * minute, less up to 20% at random so several tabs do not retry together. Five identical
 * errors in a row stop it for good; a close between them is not an error.
 */
export function liveRetry(state, event, random = Math.random) {
  const fresh = { wait: 0, attempt: 0, stopped: false, lastError: null, repeats: 0 };
  const now = state ?? fresh;
  if (event?.type === 'live') return fresh;
  if (now.stopped) return now;
  const attempt = now.attempt + 1;
  const base = Math.min(FIRST_WAIT * 2 ** (attempt - 1), MAX_WAIT);
  const spread = Math.min(Math.max(Number(random()) || 0, 0), 1);
  const wait = Math.round(base * (1 - SPREAD * spread));
  if (event?.type !== 'reset') return { ...now, wait, attempt };
  const kind = String(event.kind ?? '');
  const repeats = kind === now.lastError ? now.repeats + 1 : 1;
  return { wait, attempt, stopped: repeats >= MAX_REPEATS, lastError: kind, repeats };
}

const tag = (value) => Object.prototype.toString.call(value);

// A copy of `bytes` if they are an ArrayBuffer or a Uint8Array (from any window), else null.
function copyBytes(bytes) {
  if (tag(bytes) === '[object ArrayBuffer]') return new Uint8Array(bytes.slice(0));
  if (tag(bytes) === '[object Uint8Array]') return Uint8Array.prototype.slice.call(bytes);
  return null;
}

const position = (value) => Number.isSafeInteger(value) && value >= 0;
const shortText = (value, max) => typeof value === 'string' && value.length > 0 && value.length <= max;
const revisionOrNull = (value) => value === null || shortText(value, MAX_REVISION_CHARS);
const isObject = (value) => value !== null && typeof value === 'object' && !Array.isArray(value);

/**
 * A live message the editor posted, as `{kind, …}`, or null for anything else: another
 * editor's prefix, a kind the editor does not send, or a field missing, malformed or over its
 * limit. A checkpoint's bytes are copied, so the page keeps them whatever the editor does next.
 */
export function parseLiveEditorMessage(prefix, data) {
  if (!isObject(data) || typeof data.type !== 'string') return null;
  const start = `${prefix}:live-`;
  if (!data.type.startsWith(start)) return null;
  switch (data.type.slice(start.length)) {
    case 'send':
      return typeof data.frame === 'string' && data.frame.length <= MAX_SEND_BYTES
        ? { kind: 'send', frame: data.frame }
        : null;
    case 'reset':
      return shortText(data.kind, MAX_KIND_CHARS) ? { kind: 'reset', reset: data.kind } : null;
    case 'load':
      return revisionOrNull(data.revision) && position(data.at)
        ? { kind: 'load', revision: data.revision, at: data.at }
        : null;
    case 'checkpoint': {
      if (!shortText(data.ticket, MAX_TICKET_CHARS) || !position(data.at)) return null;
      if (!revisionOrNull(data.base) || typeof data.sha256 !== 'string' || !SHA256.test(data.sha256)) return null;
      const bytes = copyBytes(data.bytes);
      const sessionData = copyBytes(data.sessionData);
      if (!bytes || !sessionData) return null;
      return { kind: 'checkpoint', ticket: data.ticket, at: data.at, base: data.base, bytes, sessionData, sha256: data.sha256 };
    }
    case 'hash':
      return shortText(data.ticket, MAX_TICKET_CHARS) && position(data.at) && typeof data.sha256 === 'string' && SHA256.test(data.sha256)
        ? { kind: 'hash', ticket: data.ticket, at: data.at, sha256: data.sha256 }
        : null;
    case 'notice':
      return isObject(data.notice) && shortText(data.notice.kind, MAX_KIND_CHARS)
        ? { kind: 'notice', notice: data.notice }
        : null;
    case 'state':
      return shortText(data.phase, MAX_PHASE_CHARS) && position(data.waiting) && typeof data.canEdit === 'boolean'
        ? { kind: 'state', phase: data.phase, waiting: data.waiting, canEdit: data.canEdit }
        : null;
    default:
      return null;
  }
}

/**
 * The server message in `text` if it is one the page reads itself (see PEEKED), else null.
 * The type is read from the frame's start, where the server writes it, so an entry or a
 * presence note is never parsed by the page.
 */
export function peekServerFrame(text) {
  if (typeof text !== 'string') return null;
  const head = /^\{"type":"([a-z_]{1,32})"[,}]/.exec(text);
  if (!head || !PEEKED.has(head[1])) return null;
  try {
    const message = JSON.parse(text);
    return isObject(message) && message.type === head[1] ? message : null;
  } catch {
    return null;
  }
}

const integerHeader = (response, name, max) => {
  const text = response.headers.get(name);
  if (text === null || !/^-?\d{1,16}$/.test(text)) return null;
  const value = Number(text);
  return Number.isSafeInteger(value) && Math.abs(value) <= max ? value : null;
};

/**
 * Carries the live session between the socket and the editor in `frame`, from now until
 * `close()`. Call it once the editor is ready: it posts `live-start` and opens the socket.
 * `bridge` is the editor's entry in BRIDGES (`prefix`, `live` format). `handlers`:
 * - `canEdit`: whether this tab may edit;
 * - `look(person)` → `{initials, colour}` for the editor's people list;
 * - `removed(reason)`: the server removed this page (stopped for good);
 * - `stopped(why)`: the page stopped reconnecting (`repeated`, or the final server message);
 * - `peek(message)`, `access(canEdit)`, `notice(notice)`, `state(state)`: for the bar;
 * - `reloadEditor()`, `offerReload()`: the editor did not answer a tick in time;
 * - `env`: stand-ins for WebSocket, fetch, timers and random, for tests.
 */
export function connectLive(frame, bridge, docId, handlers = {}) {
  const env = {
    WebSocket: globalThis.WebSocket,
    fetch: globalThis.fetch?.bind(globalThis),
    setTimeout: globalThis.setTimeout?.bind(globalThis),
    clearTimeout: globalThis.clearTimeout?.bind(globalThis),
    setInterval: globalThis.setInterval?.bind(globalThis),
    clearInterval: globalThis.clearInterval?.bind(globalThis),
    random: Math.random,
    ...handlers.env,
  };
  const doc = frame.ownerDocument;
  const win = doc.defaultView;
  const origin = win.location.origin;
  const prefix = bridge.prefix;
  const id = encodeURIComponent(docId);
  let socket = null;
  let finished = false;
  let retry;
  let retryTimer = 0;
  let watchdog = 0;
  let reloads = 0;

  const post = (message, transfer) => {
    try {
      frame.contentWindow?.postMessage(message, origin, transfer);
    } catch {
      // The editor's window is gone; the page carries on until it is closed.
    }
  };
  const live = (kind, fields = {}) => post({ type: `${prefix}:live-${kind}`, ...fields });
  const send = (text) => {
    if (socket && socket.readyState === 1) {
      try {
        socket.send(text);
      } catch {
        // A socket closing under us; the editor sends it again after reconnecting.
      }
    }
  };

  // ---- the socket ----

  const open = () => {
    env.clearTimeout(retryTimer);
    if (finished) return;
    const scheme = win.location.protocol === 'https:' ? 'wss' : 'ws';
    const ws = new env.WebSocket(`${scheme}://${win.location.host}/ws/docs/${id}`);
    socket = ws;
    ws.onopen = () => {
      if (ws === socket && !finished) live('open');
    };
    ws.onmessage = (event) => {
      if (ws !== socket || finished || typeof event.data !== 'string') return;
      live('frame', { frame: event.data });
      const message = peekServerFrame(event.data);
      if (message) peeked(message);
    };
    ws.onclose = () => {
      if (ws !== socket) return;
      socket = null;
      if (finished) return;
      live('closed');
      again({ type: 'closed' });
    };
  };

  // Closes the socket on the page's own account, without counting the close.
  const drop = () => {
    const old = socket;
    socket = null;
    if (old) {
      old.onclose = null;
      try {
        old.close();
      } catch {
        // Already closed.
      }
    }
  };

  const again = (event) => {
    retry = liveRetry(retry, event, env.random);
    if (retry.stopped) {
      finish();
      handlers.stopped?.('repeated');
      return;
    }
    retryTimer = env.setTimeout(open, retry.wait);
  };

  // The editor (or the page) cannot follow the room: close, tell the editor, and join later.
  const reset = (kind) => {
    if (finished) return;
    drop();
    live('closed');
    again({ type: 'reset', kind });
  };

  const finish = () => {
    finished = true;
    env.clearTimeout(retryTimer);
    drop();
  };

  const peeked = (message) => {
    switch (message.type) {
      case 'removed':
        finish();
        live('stop');
        handlers.removed?.(message.reason);
        return;
      case 'people':
        people(message.people);
        break;
      case 'access':
        handlers.access?.(message.can_edit === true);
        break;
      default:
        if (FINAL.has(message.type)) {
          finish();
          handlers.stopped?.(message.type);
          return;
        }
    }
    handlers.peek?.(message);
  };

  const people = (list) => {
    if (!Array.isArray(list)) return;
    const shown = list.slice(0, MAX_PEOPLE).flatMap((person) => {
      if (!isObject(person) || !position(person.participant) || typeof person.name !== 'string') return [];
      const look = handlers.look?.(person) ?? {};
      return [
        {
          participant: person.participant,
          name: person.name,
          initials: String(look.initials ?? ''),
          colour: String(look.colour ?? ''),
          connected: person.connected === true,
        },
      ];
    });
    live('people', { people: shown });
  };

  // ---- the editor ----

  const fetchFrom = async (path, init = {}) => {
    try {
      return await env.fetch(`/api/docs/${id}${path}`, { credentials: 'same-origin', ...init });
    } catch {
      return null;
    }
  };

  const bytesOf = async (response, max) => {
    try {
      const buffer = await response.arrayBuffer();
      return buffer.byteLength <= max ? new Uint8Array(buffer) : null;
    } catch {
      return null;
    }
  };

  // The base the editor asked for: the revision's file (none: the empty document) and, for a
  // checkpoint, its session data and stamp.
  const load = async ({ revision, at }) => {
    if (revision === null) {
      live('base', { revision: null, at, bytes: new Uint8Array(0) });
      return;
    }
    const file = await fetchFrom(`/content?revision=${encodeURIComponent(revision)}`);
    const bytes = file?.status === 200 ? await bytesOf(file, MAX_BASE_BYTES) : null;
    const extra = await fetchFrom(`/live-base?at=${at}`);
    if (finished) return;
    let fields = null;
    if (bytes && extra?.status === 204) {
      fields = { revision, at, bytes };
    } else if (bytes && extra?.status === 200) {
      const sessionData = await bytesOf(extra, MAX_SESSION_DATA_BYTES);
      const atMs = integerHeader(extra, 'x-sharecraft-live-at-ms', Number.MAX_SAFE_INTEGER);
      const seed = integerHeader(extra, 'x-sharecraft-live-seed', 0xffffffff);
      if (sessionData && atMs !== null && seed !== null && seed >= 0) {
        fields = { revision, at, bytes, sessionData, stamp: { atMs, seed } };
      }
    }
    if (finished) return;
    if (!fields) {
      reset('base_fetch');
      return;
    }
    const transfer = [fields.bytes.buffer, ...(fields.sessionData ? [fields.sessionData.buffer] : [])];
    post({ type: `${prefix}:live-base`, ...fields }, transfer);
  };

  // A checkpoint the editor wrote for its ticket. A failure gives the ticket back, so the room
  // asks another editor.
  const upload = async ({ ticket, at, base, bytes, sessionData }) => {
    const query = new URLSearchParams({ ticket, at: String(at) });
    if (base !== null) query.set('base', base);
    const body = new Uint8Array(sessionData.length + bytes.length);
    body.set(sessionData, 0);
    body.set(bytes, sessionData.length);
    const response = await fetchFrom(`/live-checkpoint?${query}`, {
      method: 'PUT',
      headers: { 'x-sharecraft-session-data-length': String(sessionData.length) },
      body,
    });
    if (response?.ok !== true) send(JSON.stringify({ type: 'checkpoint_failed', ticket }));
  };

  const hash = async ({ ticket, at, sha256 }) => {
    const response = await fetchFrom('/live-checkpoint/hash', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ ticket, at, sha256 }),
    });
    if (response?.ok !== true) send(JSON.stringify({ type: 'checkpoint_failed', ticket }));
  };

  const onMessage = (event) => {
    const source = frame.contentWindow;
    if (source === null || source === undefined || event.source !== source || event.origin !== origin) return;
    if (finished) return;
    const message = parseLiveEditorMessage(prefix, event.data);
    if (!message) return;
    switch (message.kind) {
      case 'send':
        send(message.frame);
        break;
      case 'reset':
        reset(message.reset);
        break;
      case 'load':
        load(message);
        break;
      case 'checkpoint':
        upload(message);
        break;
      case 'hash':
        hash(message);
        break;
      case 'notice':
        handlers.notice?.(message.notice);
        break;
      case 'state':
        env.clearTimeout(watchdog);
        watchdog = 0;
        if (message.phase === 'live') retry = liveRetry(retry, { type: 'live' });
        handlers.state?.(message);
        break;
      default:
        break;
    }
  };

  // ---- ticks, the watchdog and leaving ----

  const tick = () => {
    if (finished) return;
    live('tick');
    if (doc.visibilityState !== 'visible' || watchdog) return;
    watchdog = env.setTimeout(() => {
      watchdog = 0;
      if (finished || doc.visibilityState !== 'visible') return;
      if (reloads < AUTO_RELOADS) {
        reloads += 1;
        handlers.reloadEditor?.();
      } else {
        handlers.offerReload?.();
      }
    }, WATCHDOG_MS);
  };
  const ticker = env.setInterval(tick, TICK_MS);

  const onPageHide = (event) => {
    if (event?.persisted || finished) return;
    send(JSON.stringify({ type: 'leave' }));
    finish();
    live('stop');
  };

  win.addEventListener('message', onMessage);
  win.addEventListener('pagehide', onPageHide);
  doc.addEventListener('visibilitychange', tick);

  live('start', { document: docId, format: bridge.live, canEdit: handlers.canEdit === true });
  open();

  return {
    /** Stops for good without leaving the place (the page is going away another way). */
    close() {
      finish();
      env.clearInterval(ticker);
      env.clearTimeout(watchdog);
      win.removeEventListener('message', onMessage);
      win.removeEventListener('pagehide', onPageHide);
      doc.removeEventListener('visibilitychange', tick);
    },
    /** Leaves the session now: the place ends and the editor is told to stop. */
    leave() {
      onPageHide({ persisted: false });
    },
  };
}
