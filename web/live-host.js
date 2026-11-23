// The frame page's side of a live session (G5): the page holds the one WebSocket and relays
// frames between it and the editor in the frame, which runs the live client. The page never
// reads a change or a presence note; it peeks only at the few server messages it acts on
// itself, waits longer after each failure, fetches the base the editor asks for, and uploads
// the editor's checkpoints and verifying hashes.
//
// The messages with the editor are `<prefix>:live-*` through postMessage between same-origin
// windows; their shapes match GridCraft's apps/gridcraft-web/src/live.rs.

/** The most bytes of one frame, in UTF-8, the editor may ask the page to send (the server's limit). */
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
/** The reset the editor asks for when the room cannot take it now; not a client fault. */
const UNAVAILABLE = 'unavailable';
/** Server messages after which the page stops for good. */
const FINAL = new Set(['incompatible', 'build_refused', 'cannot_open']);

/**
 * The wait before the next connection after `event`, as `{ wait, attempt, stopped, lastError,
 * repeats }`. Events: `{type: 'closed'}` (the socket closed), `{type: 'reset', kind}` (the
 * editor could not follow; `kind` names the error), `{type: 'live'}` (the session is live
 * again, which starts everything afresh). The wait starts at one second and doubles up to a
 * minute, less up to 20% at random so several tabs do not retry together. Five identical
 * errors in a row stop it for good; a close between them is not an error, and neither is an
 * `unavailable` room (it keeps waiting, up to a minute, until the room can take it again).
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
  if (event?.type !== 'reset' || event.kind === UNAVAILABLE) return { ...now, wait, attempt };
  const kind = String(event.kind ?? '');
  const repeats = kind === now.lastError ? now.repeats + 1 : 1;
  return { wait, attempt, stopped: repeats >= MAX_REPEATS, lastError: kind, repeats };
}

const tag = (value) => Object.prototype.toString.call(value);

// A copy of `bytes` if they are an ArrayBuffer or a Uint8Array (from any window) of at most
// `max` bytes, else null. The size is checked before anything is copied.
function copyBytes(bytes, max) {
  const kind = tag(bytes);
  if (kind !== '[object ArrayBuffer]' && kind !== '[object Uint8Array]') return null;
  if (!(bytes.byteLength <= max)) return null;
  if (kind === '[object ArrayBuffer]') return new Uint8Array(bytes.slice(0));
  return Uint8Array.prototype.slice.call(bytes);
}

/**
 * Whether `text` is at most `max` bytes in UTF-8 (as the socket sends it), counted without
 * encoding it. A lone surrogate counts three bytes, as TextEncoder writes it.
 */
function fitsBytes(text, max) {
  if (text.length > max) return false;
  if (text.length * 3 <= max) return true;
  let bytes = 0;
  for (let i = 0; i < text.length; i += 1) {
    const unit = text.charCodeAt(i);
    if (unit < 0x80) bytes += 1;
    else if (unit < 0x800) bytes += 2;
    else if (unit >= 0xd800 && unit <= 0xdbff && i + 1 < text.length) {
      const next = text.charCodeAt(i + 1);
      if (next >= 0xdc00 && next <= 0xdfff) {
        bytes += 4;
        i += 1;
      } else {
        bytes += 3;
      }
    } else bytes += 3;
    if (bytes > max) return false;
  }
  return true;
}

const position = (value) => Number.isSafeInteger(value) && value >= 0;
const shortText = (value, max) => typeof value === 'string' && value.length > 0 && value.length <= max;
const revisionOrNull = (value) => value === null || shortText(value, MAX_REVISION_CHARS);
const isObject = (value) => value !== null && typeof value === 'object' && !Array.isArray(value);

/** The most characters of typed text a notice may carry (a spreadsheet cell's limit). */
const MAX_KEPT_TEXT_CHARS = 32_767;
const MAX_NOTICE_TEXT_CHARS = 200;
const reasonText = (value) => (typeof value === 'string' && /^[a-z_]{1,64}$/.test(value) ? value : null);

/**
 * The editor's notice (GridCraft's LiveNotice) with only the fields of its kind, each checked
 * and bounded, or null. Typed text over the limit is left out rather than cut.
 */
function cleanNotice(notice) {
  if (!isObject(notice) || !shortText(notice.kind, MAX_KIND_CHARS)) return null;
  const { kind } = notice;
  switch (kind) {
    case 'not_applied': {
      const reason = reasonText(notice.reason);
      if (!position(notice.seq) || !reason) return null;
      const text = notice.kept_text;
      const kept = typeof text === 'string' && text.length <= MAX_KEPT_TEXT_CHARS ? text : null;
      return { kind, seq: notice.seq, kept_text: kept, reason };
    }
    case 'change_refused': {
      const reason = reasonText(notice.reason);
      return reason ? { kind, reason } : null;
    }
    case 'undo_left':
      return position(notice.cells) ? { kind, cells: notice.cells } : null;
    case 'undo_refused':
      return { kind, reason: shortText(notice.reason, MAX_NOTICE_TEXT_CHARS) ? notice.reason : '' };
    case 'unshared':
      return { kind, command: shortText(notice.command, MAX_NOTICE_TEXT_CHARS) ? notice.command : '' };
    case 'paused':
      return position(notice.entries) && position(notice.bytes) ? { kind, entries: notice.entries, bytes: notice.bytes } : null;
    case 'returned':
      return position(notice.not_applied) ? { kind, not_applied: notice.not_applied } : null;
    case 'others_returning':
      return position(notice.participant) ? { kind, participant: notice.participant } : null;
    case 'read_only':
      return typeof notice.read_only === 'boolean' ? { kind, read_only: notice.read_only } : null;
    case 'resumed':
    case 'archived_return':
    case 'cannot_open':
    case 'incompatible':
    case 'build_refused':
    case 'removed':
      return { kind };
    default:
      return null;
  }
}

const NOT_APPLIED = Object.freeze({
  sheet_deleted: "Your change wasn't applied because someone deleted that sheet.",
  row_deleted: "Your change wasn't applied because someone deleted that row.",
  column_deleted: "Your change wasn't applied because someone deleted that column.",
  target_unavailable: "Your change wasn't applied because the cells it was for can't be changed now.",
  changed_since_undo: "Undo wasn't applied because someone changed those cells since.",
  invalid: "Your change wasn't applied because it isn't valid here.",
  failed: "Your change couldn't be applied.",
});
const REFUSED = Object.freeze({
  read_only: "You can only view this document, so your change wasn't kept.",
  too_large: "That change is too large to share, so it wasn't kept. Try a smaller one.",
  rate_limited: "Changes came too fast, so one wasn't kept. Try it again.",
});
const PAUSED = 'Changes are paused until the latest version is saved. A lot of changes are waiting to go into the stored file.';
const KEEP_OPEN = ' Keep this tab open so they can be saved.';
const STOPS = Object.freeze({
  repeated: { text: "This document can't be opened together right now.", actions: ['reload'] },
  incompatible: { text: 'This editor is out of date. Reload to keep working together.', actions: ['reload'] },
  build_refused: { text: "Editing together isn't available in the desktop app yet.", actions: [] },
  cannot_open: {
    text: "This document can't be opened together, because one of its changes can't be applied. You can download the last stored version.",
    actions: ['download'],
  },
  not_answering: { text: "The editor isn't answering.", actions: ['reloadEditor'] },
});
const RESETS = Object.freeze({
  unavailable: 'This document is busy right now. Joining again shortly; your changes are kept.',
  // What became of the changes made while away comes later (returned, archived_return).
  expired: 'You were away for a while. Joining again.',
  base: "The latest version couldn't be loaded. Trying again shortly.",
  base_fetch: "The latest version couldn't be loaded. Trying again shortly.",
  overloaded: 'Too many changes arrived at once. Reconnecting; your changes are kept.',
});
const RECONNECTING = 'Reconnecting; your changes are kept.';
const MISMATCH = 'Two editors saved different files for the same changes. Both are in the history.';

const info = (topic, text, actions = []) => ({ topic, text, role: 'status', actions });
const stopNotice = (why) => {
  const known = Object.hasOwn(STOPS, why) ? why : 'repeated';
  const { text, actions } = STOPS[known];
  // The not-answering offer has its own topic: it goes when the editor answers again. Every
  // other stop is final.
  return { topic: known === 'not_answering' ? 'not_answering' : 'stop', text, role: 'alert', actions: [...actions] };
};
const returningNotice = (participant, context) => {
  const name = (position(participant) && context.nameOf?.(participant)) || 'someone';
  return info('return', `Changes ${name} made while away are arriving. The version before them is kept.`);
};
const pausedNotice = (context) => info('paused', PAUSED + (context.onlyEditor === true ? KEEP_OPEN : ''));
const plural = (n, one, many) => (n === 1 ? one : many);

/**
 * What the frame's banner says about something in a live session, in plain words, as
 * `{ topic, text, role, actions, copy? }`: `role` is `status` for information and `alert` for a
 * stop; `actions` at most two of `copy`, `reload`, `reloadEditor`, `download` and `activity`;
 * `copy` the typed text a Copy text action copies (never part of `text`). `{ topic, clear: true }`
 * takes that topic's banner away; null says nothing.
 *
 * `source` is `editor` (a LiveNotice, as parseLiveEditorMessage cleaned it), `server` (a peeked
 * server message), `reset` (a reset kind: the editor joins again, never shown as an error) or
 * `stop` (why the page stopped). `context`: `nameOf(participant)`, `onlyEditor` (this tab is the
 * only one that could save) and `canManage` (this person manages the document).
 */
export function liveNotice(source, value, context = {}) {
  if (source === 'stop') return stopNotice(String(value));
  if (source === 'reset') {
    const kind = String(value);
    return info('connection', Object.hasOwn(RESETS, kind) ? RESETS[kind] : RECONNECTING);
  }
  if (!isObject(value)) return null;
  if (source === 'server') {
    switch (value.type) {
      case 'changes_paused':
        return pausedNotice(context);
      case 'changes_resumed':
        return { topic: 'paused', clear: true };
      case 'returning':
        return returningNotice(value.participant, context);
      case 'cannot_open':
      case 'incompatible':
      case 'build_refused':
        return stopNotice(value.type);
      case 'saved':
        // Owners only: two editors wrote different files for one position (C6).
        return value.mismatch === true && context.canManage === true ? info('mismatch', MISMATCH, ['activity']) : null;
      default:
        return null;
    }
  }
  if (source !== 'editor') return null;
  switch (value.kind) {
    case 'not_applied': {
      const text = Object.hasOwn(NOT_APPLIED, value.reason) ? NOT_APPLIED[value.reason] : NOT_APPLIED.failed;
      const kept = typeof value.kept_text === 'string' && value.kept_text !== '' ? value.kept_text : null;
      if (!kept) return info('change', text);
      return { ...info('change', `${text} Your text is kept.`, ['copy']), copy: kept };
    }
    case 'change_refused':
      return info('change', Object.hasOwn(REFUSED, value.reason) ? REFUSED[value.reason] : "Your change couldn't be shared, so it wasn't kept.");
    case 'undo_left': {
      const n = value.cells;
      return info('change', `Undo left ${n} ${plural(n, 'cell as it was', 'cells as they were')}, because someone changed ${plural(n, 'it', 'them')} since.`);
    }
    case 'undo_refused':
      return info('change', "Undo isn't possible here, because someone else's changes depend on it.");
    case 'unshared':
      return info('change', "That command isn't available while editing together.");
    case 'paused':
      return pausedNotice(context);
    case 'resumed':
      return { topic: 'paused', clear: true };
    case 'returned': {
      const n = value.not_applied;
      return info(
        'return',
        n > 0 ? `Welcome back. ${n} of the changes you made while away couldn't be applied.` : 'Welcome back. The changes you made while away have been added.',
      );
    }
    case 'archived_return':
      return info('return', "You were away too long for your changes to be added. The copy you had is kept in the file's history.");
    case 'others_returning':
      return returningNotice(value.participant, context);
    case 'read_only':
      return info('access', value.read_only ? 'You can now only view this document.' : 'You can edit this document again.');
    case 'cannot_open':
    case 'incompatible':
    case 'build_refused':
      return stopNotice(value.kind);
    default:
      // `removed` is shown by the page's own removal; anything else says nothing.
      return null;
  }
}

/**
 * The one banner once `next` arrives with `current` showing: the newest wins, except that a stop
 * (`alert`) gives way only to another stop; a clear takes away only its own topic. At most two
 * actions are kept.
 */
export function pickBanner(current, next) {
  if (!next) return current ?? null;
  if (next.clear) return current?.topic === next.topic ? null : (current ?? null);
  if (current?.role === 'alert' && next.role !== 'alert') return current;
  return next.actions.length > 2 ? { ...next, actions: next.actions.slice(0, 2) } : next;
}

const WAITING_LOST = " Changes made here since then weren't sent: download your copy before you reload.";

/**
 * The one banner the frame shows: `pageNotice` the page's own (an editor that did not start, an
 * error), `live` the live session's (pickBanner), `waiting` the changes the editor still holds
 * that the server has not taken. A live stop is never covered by a page notice; otherwise the
 * page's notice comes first. A stop whose way on is Reload, with changes waiting, says they were
 * not sent and offers the person's copy before the reload.
 */
export function shownBanner(pageNotice, live, waiting) {
  const stopped = live?.role === 'alert';
  const shown = stopped ? live : (pageNotice ?? live ?? null);
  if (!stopped || shown !== live || !(waiting > 0) || !live.actions.includes('reload')) return shown;
  return { ...live, text: live.text + WAITING_LOST, actions: ['mycopy', 'reload'] };
}

/**
 * Whether this tab has changes the stored file may not hold, from what the page can see without
 * reading an entry: `{type: 'sent'}` (this tab sent a change) and `{type: 'saved', through}` (a
 * version was stored up to `through`). The page never learns the position of its own changes,
 * so it counts conservatively: a change is covered by the first stored version whose position
 * is beyond that of the one stored after the change was sent. A change sent before a version
 * arrived may lie beyond it, so it waits for the next one; a repeat for a position already seen
 * (a verification) moves nothing on. Answers `{ unsaved, sentSince, through }`.
 */
export function trackUnsaved(track, event) {
  const now = track ?? { unsaved: false, sentSince: false, through: -1 };
  if (event?.type === 'sent') return { ...now, unsaved: true, sentSince: true };
  if (event?.type !== 'saved' || !position(event.through) || event.through <= now.through) return now;
  return { unsaved: now.sentSince, sentSince: false, through: event.through };
}

/**
 * Whether this tab is the only connected editor that could save the latest version: `people` is
 * the room's list (this tab among them), `canEdit` this tab's right. Viewers and places whose
 * connection dropped do not count.
 */
export function onlyEditor(people, canEdit) {
  if (canEdit !== true) return false;
  const list = Array.isArray(people) ? people : [];
  return list.filter((person) => isObject(person) && person.can_edit === true && person.connected === true).length <= 1;
}

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
      return typeof data.frame === 'string' && fitsBytes(data.frame, MAX_SEND_BYTES)
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
      const bytes = copyBytes(data.bytes, MAX_BASE_BYTES);
      const sessionData = copyBytes(data.sessionData, MAX_SESSION_DATA_BYTES);
      if (!bytes || !sessionData) return null;
      return { kind: 'checkpoint', ticket: data.ticket, at: data.at, base: data.base, bytes, sessionData, sha256: data.sha256 };
    }
    case 'hash': {
      if (!shortText(data.ticket, MAX_TICKET_CHARS) || !position(data.at)) return null;
      if (typeof data.sha256 !== 'string' || !SHA256.test(data.sha256)) return null;
      // The server needs the identity data's hash too (G4). An editor that does not send it
      // yet is passed through as it is, and the server refuses the hash.
      if (data.sessionSha256 === undefined) return { kind: 'hash', ticket: data.ticket, at: data.at, sha256: data.sha256 };
      if (typeof data.sessionSha256 !== 'string' || !SHA256.test(data.sessionSha256)) return null;
      return { kind: 'hash', ticket: data.ticket, at: data.at, sha256: data.sha256, sessionSha256: data.sessionSha256 };
    }
    case 'notice': {
      const notice = cleanNotice(data.notice);
      return notice ? { kind: 'notice', notice } : null;
    }
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

/** Whether leaving the page now loses changes: the editor's last state had some waiting. */
export function warnOnLeave(state) {
  return Number.isSafeInteger(state?.waiting) && state.waiting > 0;
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
 * - `reset(kind)`: the editor could not follow and joins again after a wait;
 * - `opened()`: a new socket is open (the room repeats any pause to a joiner);
 * - `changed()`: this tab sent one of its changes; `saving(on)`: a checkpoint upload started or
 *   the last one ended;
 * - `reloadEditor()`, `offerReload()`: the editor did not answer a tick in time;
 * - `memory`: an object the page keeps across editor reloads, which holds the back-off state
 *   and the count of automatic reloads, so neither starts again with a new editor; both reset
 *   when the session is live;
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
  const memory = handlers.memory ?? {};
  memory.reloads ??= 0;
  let socket = null;
  let generation = 0; // counts sockets, so a late answer for an older one is dropped
  let finished = false;
  let retryTimer = 0;
  let watchdog = 0;
  let uploads = 0; // checkpoint uploads under way

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
    generation += 1;
    ws.onopen = () => {
      if (ws !== socket || finished) return;
      live('open');
      handlers.opened?.();
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
    memory.retry = liveRetry(memory.retry, event, env.random);
    // A close and a reset can come together; only the latest wait opens a socket.
    env.clearTimeout(retryTimer);
    if (memory.retry.stopped) {
      finish();
      handlers.stopped?.('repeated');
      return;
    }
    retryTimer = env.setTimeout(open, memory.retry.wait);
  };

  // The editor (or the page) cannot follow the room: close, tell the editor, and join later.
  const reset = (kind) => {
    if (finished) return;
    // The socket may be gone already (the server closed it after its last frame).
    if (socket) {
      drop();
      live('closed');
    }
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
    const asked = generation;
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
    // An answer for an older socket is dropped: its editor asks again after the catch-up.
    if (finished || asked !== generation) return;
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
    // One body, session data first, without copying the workbook a third time.
    const body = new Blob([sessionData, bytes]);
    uploads += 1;
    if (uploads === 1) handlers.saving?.(true);
    const response = await fetchFrom(`/live-checkpoint?${query}`, {
      method: 'PUT',
      headers: { 'x-sharecraft-session-data-length': String(sessionData.length) },
      body,
    });
    uploads -= 1;
    if (uploads === 0) handlers.saving?.(false);
    if (response?.ok !== true) send(JSON.stringify({ type: 'checkpoint_failed', ticket }));
  };

  const hash = async ({ ticket, at, sha256, sessionSha256 }) => {
    const body = sessionSha256 === undefined ? { ticket, at, sha256 } : { ticket, at, sha256, session_sha256: sessionSha256 };
    const response = await fetchFrom('/live-checkpoint/hash', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(body),
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
        // Only the frame's type is read, from its start, as the server's are: an entry is one
        // of this tab's changes (a presence note or a heartbeat is not).
        if (message.frame.startsWith('{"type":"entry"')) handlers.changed?.();
        break;
      case 'reset':
        handlers.reset?.(message.reset);
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
        if (message.phase === 'live') {
          memory.retry = liveRetry(memory.retry, { type: 'live' });
          memory.reloads = 0;
        }
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
      if (memory.reloads < AUTO_RELOADS) {
        memory.reloads += 1;
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
