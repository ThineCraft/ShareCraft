// The bridge between the frame page and an editor running in its <iframe>, and the small pure
// pieces the frame decides with. Every exported function except `connect` is pure: no window,
// no document, no network. The page script (file.js) does the rest.

/** The editors that open file documents: their names and the extension of their own files. */
export const EDITORS = Object.freeze({
  text: Object.freeze({ name: 'Text', extension: null }),
  gridcraft: Object.freeze({ name: 'GridCraft', extension: '.xlsx' }),
  wordcraft: Object.freeze({ name: 'WordCraft', extension: '.docx' }),
  pdfcraft: Object.freeze({ name: 'PdfCraft', extension: '.pdf' }),
  solidcraft: Object.freeze({ name: 'SolidCraft', extension: '.solidcraft.json' }),
});

/** How the page talks to each editor, by editor id. */
export const BRIDGES = Object.freeze({
  gridcraft: Object.freeze({ kind: 'craft-web-host', prefix: 'gridcraft', query: 'host=parent' }),
  wordcraft: Object.freeze({ kind: 'craft-web-host', prefix: 'wordcraft', query: 'host=parent' }),
  pdfcraft: Object.freeze({ kind: 'craft-web-host', prefix: 'pdfcraft', query: 'host=parent&control' }),
  solidcraft: Object.freeze({ kind: 'solidcraft-request', query: 'embed=editor' }),
});

const own = (object, key) => typeof key === 'string' && Object.hasOwn(object, key);

/** The address of editor `id`, hosted by this page, with the author's name; null for no editor. */
export function editorUrl(id, authorName) {
  if (!own(BRIDGES, id)) return null;
  const author = encodeURIComponent(String(authorName ?? ''));
  return `/editors/${encodeURIComponent(id)}/index.html?${BRIDGES[id].query}&author=${author}`;
}

// Lower-cases ASCII letters only, so that no other character can turn into one.
const asciiLower = (text) => text.replace(/[A-Z]/g, (c) => String.fromCharCode(c.charCodeAt(0) + 32));

/**
 * What a `save` from the editor is: "save" (the document itself) only when `name` ends with the
 * document's native extension, ignoring ASCII case; anything else is "export", a file for the
 * person to keep, which never touches the document.
 */
export function classifySave(nativeExtension, name) {
  if (typeof nativeExtension !== 'string' || nativeExtension === '') return 'export';
  if (typeof name !== 'string') return 'export';
  return asciiLower(name).endsWith(asciiLower(nativeExtension)) ? 'save' : 'export';
}

/** `name`, trimmed, ending with `extension` (added unless it is there in any ASCII case). */
export function withExtension(name, extension) {
  const trimmed = String(name ?? '').trim() || 'Untitled';
  return classifySave(extension, trimmed) === 'save' ? trimmed : `${trimmed}${extension}`;
}

const tag = (value) => Object.prototype.toString.call(value);

// A copy of `bytes` if they are an ArrayBuffer or a Uint8Array (from any window), else null.
function copyBytes(bytes) {
  if (tag(bytes) === '[object ArrayBuffer]') return new Uint8Array(bytes.slice(0));
  if (tag(bytes) === '[object Uint8Array]') return Uint8Array.prototype.slice.call(bytes);
  return null;
}

/**
 * A message the editor posted, as `{kind: "ready"|"save"|"dirty"|"failed", …}`, or null for
 * anything else: another editor's prefix, a kind the editor does not send, or a malformed body.
 * A save's bytes are copied, so the page keeps them whatever the editor does next.
 */
export function parseHostMessage(prefix, data) {
  if (data === null || typeof data !== 'object' || Array.isArray(data)) return null;
  if (typeof data.type !== 'string' || !data.type.startsWith(`${prefix}:`)) return null;
  const kind = data.type.slice(prefix.length + 1);
  switch (kind) {
    case 'ready':
      return { kind, version: typeof data.version === 'string' ? data.version : '1' };
    case 'save': {
      const bytes = copyBytes(data.bytes);
      if (!bytes) return null;
      const name = typeof data.name === 'string' ? data.name : '';
      return { kind, name, bytes, saveAs: data.saveAs === true };
    }
    case 'dirty':
      return typeof data.dirty === 'boolean' ? { kind, dirty: data.dirty } : null;
    case 'failed':
      return { kind, error: typeof data.error === 'string' ? data.error : '' };
    default:
      return null;
  }
}

/**
 * The frame's status line after `event`. `state` is the previous result (undefined at the
 * start). The result holds what the frame knows and the words to show (`text`) with a `tone`
 * for the colour, which never stands alone. Events:
 * opening; opened {canSave}; seat {canSave}; dirty {dirty}; saving (the editor saved, the
 * upload is under way); stored; not_stored; offline; online.
 */
export function nextStatus(state, event) {
  const s = {
    opened: false,
    canSave: false,
    dirty: false,
    saving: false,
    failed: false,
    offline: false,
    ...(state ?? {}),
  };
  switch (event?.type) {
    case 'opening':
      Object.assign(s, { opened: false, dirty: false, saving: false, failed: false });
      break;
    case 'opened':
      Object.assign(s, { opened: true, canSave: event.canSave === true });
      break;
    case 'seat':
      s.canSave = event.canSave === true;
      break;
    case 'dirty':
      s.dirty = event.dirty === true;
      break;
    case 'saving':
      // The editor counts its document saved once it hands the bytes over.
      Object.assign(s, { dirty: false, saving: true, failed: false });
      break;
    case 'stored':
      Object.assign(s, { saving: false, failed: false });
      break;
    case 'not_stored':
      Object.assign(s, { saving: false, failed: true });
      break;
    case 'offline':
      s.offline = true;
      break;
    case 'online':
      s.offline = false;
      break;
    default:
      break;
  }
  const [text, tone] = s.offline
    ? ['Connection lost. Saving is paused.', 'offline']
    : !s.opened
      ? ['Opening…', 'connecting']
      : !s.canSave
        ? ['View only', 'view']
        : s.saving
          ? ['Saving…', 'saving']
          : s.failed
            ? ['Not saved', 'failed']
            : s.dirty
              ? ['Unsaved changes', 'notice']
              : ['Saved', 'saved'];
  return { ...s, text, tone };
}

/**
 * The saves on their way to the server: one upload at a time, and at most one save waiting
 * behind it (a newer save replaces the waiting one). `upload(bytes)` resolves to `{ok: true}`
 * once the server stored them, or to `{ok: false, status}`; a rejection counts as a network
 * failure (status 0). Bytes are kept until the server stored them; a refused save stays kept
 * until `retry()` or a newer save. `onEvent` hears "saving", "stored" and "not_stored".
 */
export class SaveQueue {
  #upload;
  #onEvent;
  #inFlight = null; // bytes being uploaded
  #waiting = null;  // the newest bytes not yet sent
  #failed = null;   // { bytes, result } of the last refused upload
  #paused = false;
  #dropped = false;

  constructor(upload, onEvent = () => {}) {
    this.#upload = upload;
    this.#onEvent = onEvent;
  }

  /** The newest bytes the server has not stored yet, or null. */
  get kept() {
    return this.#waiting ?? this.#inFlight ?? this.#failed?.bytes ?? null;
  }

  /** The answer to the last refused upload while its bytes are still kept, or null. */
  get failure() {
    return this.#failed?.result ?? null;
  }

  /** Whether an upload is under way. */
  get busy() {
    return this.#inFlight !== null;
  }

  /** A new save from the editor. */
  offer(bytes) {
    if (this.#dropped) return;
    this.#failed = null;
    this.#waiting = bytes;
    this.#next();
  }

  /** Uploads the refused bytes again. */
  retry() {
    if (this.#dropped || !this.#failed) return;
    if (this.#waiting === null) this.#waiting = this.#failed.bytes;
    this.#failed = null;
    this.#next();
  }

  /** Holds new uploads back (one under way finishes). */
  pause() {
    this.#paused = true;
  }

  resume() {
    this.#paused = false;
    this.#next();
  }

  /** Forgets every kept byte and takes nothing more. */
  drop() {
    this.#dropped = true;
    this.#inFlight = null;
    this.#waiting = null;
    this.#failed = null;
  }

  #next() {
    if (this.#dropped || this.#paused || this.#inFlight !== null || this.#waiting === null) return;
    const bytes = this.#waiting;
    this.#waiting = null;
    this.#inFlight = bytes;
    this.#onEvent({ type: 'saving' });
    let answer;
    try {
      answer = Promise.resolve(this.#upload(bytes));
    } catch {
      answer = Promise.resolve({ ok: false, status: 0 });
    }
    answer
      .catch(() => ({ ok: false, status: 0 }))
      .then((result) => this.#settled(bytes, result ?? { ok: false, status: 0 }));
  }

  #settled(bytes, result) {
    if (this.#dropped || this.#inFlight !== bytes) return;
    this.#inFlight = null;
    if (result.ok) {
      this.#onEvent({ type: 'stored', result });
    } else if (this.#waiting === null) {
      this.#failed = { bytes, result };
      this.#onEvent({ type: 'not_stored', result });
      return;
    } else {
      // A newer save is waiting: it carries everything the refused one did.
      this.#onEvent({ type: 'not_stored', result });
    }
    this.#next();
  }
}

/**
 * Wires `frame` (an <iframe> whose page has not started loading yet) to its editor through
 * `bridge` (one of BRIDGES). `handlers` are called with the parsed messages: ready, save,
 * dirty and failed (`{error}`). Returns `{ open(name, bytes), requestSave(), close() }`.
 * New kinds of bridge are added to BRIDGE_KINDS; callers do not change.
 */
export function connect(frame, bridge, handlers) {
  const make = own(BRIDGE_KINDS, bridge?.kind) ? BRIDGE_KINDS[bridge.kind] : notAvailable;
  return make(frame, bridge, handlers);
}

const BRIDGE_KINDS = Object.freeze({
  'craft-web-host': craftWebHost,
  // Task 10 brings SolidCraft's own bridge.
  'solidcraft-request': notAvailable,
});

function notAvailable(_frame, _bridge, handlers) {
  queueMicrotask(() => handlers.failed?.({ error: "This editor's bridge is not available yet." }));
  return { open() {}, requestSave: () => false, close() {} };
}

// The craft-web-host contract: messages `{type: "<prefix>:<kind>", …}` through postMessage,
// between this page and the same-origin editor in the frame.
function craftWebHost(frame, bridge, handlers) {
  const win = frame.ownerDocument.defaultView;
  const origin = win.location.origin;
  const listener = (event) => {
    if (event.source !== frame.contentWindow || event.origin !== origin) return;
    const message = parseHostMessage(bridge.prefix, event.data);
    if (message) handlers[message.kind]?.(message);
  };
  win.addEventListener('message', listener);
  return {
    open(name, bytes) {
      // Posted as a copy: the page keeps its own bytes.
      frame.contentWindow?.postMessage({ type: `${bridge.prefix}:open`, name, bytes }, origin);
    },
    requestSave() {
      try {
        return frame.contentWindow?.[bridge.prefix]?.execute('file.save', {}) !== false;
      } catch {
        return false;
      }
    },
    close() {
      win.removeEventListener('message', listener);
    },
  };
}
