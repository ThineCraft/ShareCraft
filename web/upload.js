// Uploading: files (and whole folders) sent to the server in 8 MiB chunks through upload
// sessions, each checked against a SHA-256 computed here, in the browser, while the file is read
// a slice at a time (a file of many gigabytes is never held in memory). A dropped connection is
// retried from the offset the server reports. Nothing here touches the page: the library page
// shows the queue's progress and answers its questions. Nothing runs at import.

import { bytesText, goToSignIn, messageFor, quotaRefusalText, seg } from './common.js';

/** The size of one chunk: 8 MiB. */
export const CHUNK = 8 * 1024 * 1024;

/**
 * The chunks still to send for a file of `size` bytes of which the server holds `received`:
 * `[{ start, end }]` with `end` the last byte included. The server keeps a chunk's first part
 * when it breaks, so `received` need not be on a chunk boundary.
 */
export function planChunks(size, received = 0, chunk = CHUNK) {
  const chunks = [];
  for (let start = received; start < size; start += chunk) {
    chunks.push({ start, end: Math.min(start + chunk, size) - 1 });
  }
  return chunks;
}

/** The `Content-Range` header for `chunk` of a file of `size` bytes. */
export const contentRange = (chunk, size) => `bytes ${chunk.start}-${chunk.end}/${size}`;

/** What can be done about a name that is taken. */
export const CONFLICT_CHOICES = Object.freeze([
  Object.freeze({ value: 'replace', label: 'Replace' }),
  Object.freeze({ value: 'keep_both', label: 'Keep both' }),
  Object.freeze({ value: 'skip', label: 'Skip' }),
]);

/** The question asked when `name` is taken; `suggested` is the name Keep both would give. */
export const conflictQuestion = (name, suggested) =>
  `A file named “${name}” is already here. Replace it with a new version, keep both (this one is saved as “${suggested}”), or skip this file?`;

/** The folders and the file name in a path such as `Photos/2026/a.png`. */
export function splitRelative(path) {
  const parts = String(path).split('/').filter((part) => part !== '');
  const name = parts.pop() ?? '';
  return { folders: parts, name };
}

/** The sentence under a finished upload: what happened to its name, or that it was skipped. */
export function outcomeNote(outcome) {
  if (outcome.status === 'skipped') return 'Skipped: a file with that name was already there.';
  if (outcome.status === 'failed') return outcome.message ?? '';
  const parts = [];
  if (outcome.renamed) {
    parts.push(
      `“${outcome.renamed.from}” was saved as “${outcome.renamed.to}”: the characters / \\ : * ? " < > | are not allowed in names.`,
    );
  }
  if (outcome.kept) {
    parts.push(`“${outcome.kept.from}” already existed, so this one was saved as “${outcome.kept.to}”.`);
  }
  return parts.length ? parts.join(' ') : 'Uploaded.';
}

// ---- SHA-256, incrementally ----

const K = new Uint32Array([
  0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01,
  0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc,
  0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147,
  0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
  0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08,
  0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
  0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
]);

const rotr = (x, n) => (x >>> n) | (x << (32 - n));

/** SHA-256 of bytes fed in pieces: `update` any number of times, then `digestHex` once. */
export class Sha256 {
  constructor() {
    this.state = new Uint32Array([
      0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ]);
    this.pending = new Uint8Array(64);
    this.held = 0; // bytes waiting in `pending`
    this.total = 0; // bytes fed in
    this.words = new Uint32Array(64);
  }

  /** Feeds `bytes` (a Uint8Array). */
  update(bytes) {
    this.total += bytes.length;
    let at = 0;
    if (this.held > 0) {
      const take = Math.min(64 - this.held, bytes.length);
      this.pending.set(bytes.subarray(0, take), this.held);
      this.held += take;
      at = take;
      if (this.held < 64) return;
      this.#block(this.pending, 0);
      this.held = 0;
    }
    while (at + 64 <= bytes.length) {
      this.#block(bytes, at);
      at += 64;
    }
    if (at < bytes.length) {
      this.pending.set(bytes.subarray(at), 0);
      this.held = bytes.length - at;
    }
  }

  /** The 64 lowercase hexadecimal digits of the hash of everything fed in. */
  digestHex() {
    const total = this.total;
    const padding = new Uint8Array((this.held < 56 ? 56 : 120) - this.held + 8);
    padding[0] = 0x80;
    const view = new DataView(padding.buffer);
    view.setUint32(padding.length - 8, Math.floor(total / 0x20000000));
    view.setUint32(padding.length - 4, (total % 0x20000000) * 8);
    this.update(padding);
    return [...this.state].map((word) => word.toString(16).padStart(8, '0')).join('');
  }

  #block(bytes, offset) {
    const w = this.words;
    for (let i = 0; i < 16; i += 1) {
      const at = offset + i * 4;
      w[i] = (bytes[at] << 24) | (bytes[at + 1] << 16) | (bytes[at + 2] << 8) | bytes[at + 3];
    }
    for (let i = 16; i < 64; i += 1) {
      const s0 = rotr(w[i - 15], 7) ^ rotr(w[i - 15], 18) ^ (w[i - 15] >>> 3);
      const s1 = rotr(w[i - 2], 17) ^ rotr(w[i - 2], 19) ^ (w[i - 2] >>> 10);
      w[i] = (w[i - 16] + s0 + w[i - 7] + s1) | 0;
    }
    let [a, b, c, d, e, f, g, h] = this.state;
    for (let i = 0; i < 64; i += 1) {
      const t1 = (h + (rotr(e, 6) ^ rotr(e, 11) ^ rotr(e, 25)) + ((e & f) ^ (~e & g)) + K[i] + w[i]) | 0;
      const t2 = ((rotr(a, 2) ^ rotr(a, 13) ^ rotr(a, 22)) + ((a & b) ^ (a & c) ^ (b & c))) | 0;
      h = g;
      g = f;
      f = e;
      e = (d + t1) | 0;
      d = c;
      c = b;
      b = a;
      a = (t1 + t2) | 0;
    }
    const s = this.state;
    s[0] += a;
    s[1] += b;
    s[2] += c;
    s[3] += d;
    s[4] += e;
    s[5] += f;
    s[6] += g;
    s[7] += h;
  }
}

/** The SHA-256 of `file` (anything with `size` and `slice`), read a chunk at a time. */
export async function hashFile(file, onProgress = () => {}) {
  const hash = new Sha256();
  for (let start = 0; start < file.size; start += CHUNK) {
    const end = Math.min(start + CHUNK, file.size);
    hash.update(new Uint8Array(await file.slice(start, end).arrayBuffer()));
    onProgress(end);
  }
  return hash.digestHex();
}

// ---- the queue ----

const sleepFor = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/** A failure that ends one file's upload, with the sentence to show. */
/** Whether the browser failed to read the picked file (moved, deleted, locked or permission lost). */
const unreadable = (e) => ['NotReadableError', 'NotFoundError', 'SecurityError'].includes(e?.name);
const readFailure = (name) =>
  `“${name}” could not be read. It may have been moved, deleted or be in use by another program. Check it and pick it again.`;

class Stop extends Error {
  /** `keep`: the session is still open and a retry can finish it; `ended`: nothing to give back. */
  constructor(message, retry = false, { keep = false, ended = false } = {}) {
    super(message);
    this.retry = retry;
    this.keep = keep;
    this.ended = ended;
  }
}

/**
 * A queue of files to upload one after another. Each is `{ name, size, slice, path?, version? }`:
 * `path` the folders to make (or find) below `parent` first, `version` (`{ doc, base }`) a new
 * version of an existing file instead of a new file.
 *
 * Options: `parent` (the library or folder), `site` (for explaining a quota refusal), `onChange`
 * (called with the file's outcome whenever it changes), `resolve` (called with `{ name,
 * suggested }` when a name is taken, answering `{ choice, all }`), `attempts` (how many
 * failed requests in a row end a file), and `fetch`, `hash`, `sleep` (for tests).
 */
export class UploadQueue {
  constructor({
    parent,
    site,
    onChange = () => {},
    resolve = null,
    attempts = 5,
    fetch: fetcher = (...args) => globalThis.fetch(...args),
    hash = hashFile,
    sleep = sleepFor,
  } = {}) {
    this.parent = parent;
    this.site = site;
    this.onChange = onChange;
    this.resolve = resolve;
    this.attempts = attempts;
    this.fetch = fetcher;
    this.hash = hash;
    this.sleep = sleep;
    this.rest = null; // the choice made "for the rest"
    this.folders = new Map();
  }

  /** Uploads `files` in order; resolves with one outcome per file, never rejects. */
  async run(files) {
    const outcomes = files.map((file) => ({
      file,
      name: file.name,
      size: file.size,
      sent: 0,
      status: 'waiting',
      message: '',
      renamed: null,
      kept: null,
      retry: false,
    }));
    for (const outcome of outcomes) this.onChange(outcome);
    for (const outcome of outcomes) {
      try {
        await this.#one(outcome);
      } catch (e) {
        outcome.status = 'failed';
        outcome.message = e instanceof Stop ? e.message : unreadable(e) ? readFailure(outcome.name) : messageFor('unknown');
        outcome.retry = e instanceof Stop && (e.retry || e.keep);
        outcome.keep = e instanceof Stop && e.keep;
      }
      this.#change(outcome);
    }
    return outcomes;
  }

  #change(outcome, fields = {}) {
    Object.assign(outcome, fields);
    this.onChange(outcome);
  }

  /** One request. Never throws: `{ ok, status, data, error }`, `error` `network` if it did not arrive. */
  async #call(method, url, init = {}) {
    let response;
    try {
      response = await this.fetch(url, { method, credentials: 'same-origin', ...init });
    } catch {
      return { ok: false, status: 0, data: null, error: 'network' };
    }
    let data = null;
    if (response.status !== 204) {
      try {
        data = await response.json();
      } catch {
        data = null;
      }
    }
    const error = response.ok ? null : typeof data?.error === 'string' ? data.error : 'unknown';
    if (response.status === 401 && error === 'not_signed_in') goToSignIn();
    return { ok: response.ok, status: response.status, data, error };
  }

  #json(method, url, body) {
    return this.#call(method, url, {
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    });
  }

  async #one(outcome) {
    const { file } = outcome;
    if (file.size === 0) throw new Stop(`“${file.name}” is empty. Empty files cannot be uploaded.`);
    let id = file.resumeId;
    let cleaned = null;
    if (id) {
      // A kept session: ask the server how much it holds, then send the rest and finish.
      const asked = await this.#call('GET', `/api/uploads/${seg(id)}`);
      if (asked.status === 404) {
        id = null; // it ended on the server (24 h, or a restart that lost it): start afresh
      } else if (!asked.ok || typeof asked.data?.received !== 'number') {
        throw new Stop(messageFor(asked.error ?? 'network'), true, { keep: true });
      } else {
        outcome.parentId = file.parentId;
        outcome.sessionId = id;
        this.#change(outcome, { status: 'uploading', message: '', sent: Math.min(asked.data.received, file.size) });
      }
    }
    if (!id) {
      this.#change(outcome, { status: 'hashing', message: 'Checking the file…', hashed: 0 });
      const sha256 = await this.hash(file, (done) => this.#change(outcome, { sent: 0, hashed: done }));
      const parent = file.version ? null : await this.#folderFor(file.path ?? []);
      outcome.parentId = parent;
      const body = file.version
        ? { doc: file.version.doc, base: file.version.base, size: file.size, sha256 }
        : { parent, name: file.name, size: file.size, sha256 };
      const started = await this.#start(body, file);
      id = started.id;
      outcome.sessionId = id;
      cleaned = started.renamed ?? null;
      this.#change(outcome, { status: 'uploading', message: '', sent: started.received ?? 0 });
    }
    try {
      if (outcome.sent < file.size) await this.#send(outcome, id);
      await this.#finish(outcome, id, cleaned);
    } catch (e) {
      // A session left open keeps its size reserved against the site's quota: give it back,
      // unless it ended already or is being kept for a retry.
      if (!(e instanceof Stop && (e.keep || e.ended))) await this.#call('DELETE', `/api/uploads/${seg(id)}`);
      throw e;
    }
  }

  async #start(body, file) {
    let failures = 0;
    for (;;) {
      const result = await this.#json('POST', '/api/uploads', body);
      if (result.ok) return result.data;
      if (result.error === 'network' && (failures += 1) < this.attempts) {
        await this.sleep(this.#backoff(failures));
        continue;
      }
      throw await this.#problem(result, file);
    }
  }

  #backoff(failures) {
    return Math.min(30_000, 500 * 2 ** failures);
  }

  /** Sends every chunk the server lacks, resuming from its offset after any interruption. */
  async #send(outcome, id) {
    const { file } = outcome;
    const url = `/api/uploads/${seg(id)}`;
    let received = outcome.sent;
    let failures = 0;
    while (received < file.size) {
      const chunk = planChunks(file.size, received)[0];
      const result = await this.#call('PUT', url, {
        headers: { 'Content-Range': contentRange(chunk, file.size), 'Content-Type': 'application/octet-stream' },
        body: file.slice(chunk.start, chunk.end + 1),
      });
      if (result.ok) {
        received = result.data.received;
        failures = 0;
        this.#change(outcome, { sent: received });
        continue;
      }
      if (result.error === 'wrong_offset' && typeof result.data?.expected === 'number') {
        received = result.data.expected;
      } else if (result.error === 'incomplete_chunk' && typeof result.data?.received === 'number') {
        // Part of the chunk arrived and was kept; only that counts as progress.
        if (result.data.received <= received) failures += 1;
        received = result.data.received;
      } else if (result.error === 'network' || result.status >= 500) {
        failures += 1;
      } else if (result.status === 404) {
        throw new Stop(`The upload of “${file.name}” ended on the server. Upload it again.`, true);
      } else {
        throw await this.#problem(result, file);
      }
      this.#change(outcome, { sent: received });
      if (failures >= this.attempts) throw new Stop(messageFor('network'), true, { keep: true });
      if (failures > 0) {
        await this.sleep(this.#backoff(failures));
        // The server's own count is the truth about what it holds.
        const asked = await this.#call('GET', url);
        if (asked.ok && typeof asked.data?.received === 'number') received = asked.data.received;
        else if (asked.status === 404) {
          throw new Stop(`The upload of “${file.name}” ended on the server. Upload it again.`, true);
        }
      }
    }
  }

  /** Finishes the session, asking what to do if the name is taken. */
  async #finish(outcome, id, cleaned) {
    const { file } = outcome;
    let choice = file.resumeChoice ?? this.rest;
    let failures = 0;
    let lost = false; // an answer to the finish may have been lost on the way
    for (;;) {
      outcome.choice = choice;
      const query = choice ? `?on_conflict=${choice}` : '';
      const result = await this.#call('POST', `/api/uploads/${seg(id)}/finish${query}`);
      if (result.ok) {
        if (result.data?.skipped) return this.#change(outcome, { status: 'skipped', message: '' });
        const data = result.data ?? {};
        const kept = choice === 'keep_both' && data.renamed ? data.renamed : null;
        return this.#change(outcome, {
          status: 'done',
          sent: file.size,
          name: data.name ?? outcome.name,
          renamed: kept ? cleaned : (cleaned ?? data.renamed ?? null),
          kept,
          message: '',
        });
      }
      if (result.error === 'network') lost = true;
      if (result.error === 'network' && (failures += 1) < this.attempts) {
        await this.sleep(this.#backoff(failures));
        continue;
      }
      if (lost && result.status === 404) return this.#settleLost(outcome, cleaned);
      if (result.error === 'name_taken' && this.resolve && !choice) {
        const answer = await this.resolve({ name: file.name, suggested: result.data?.suggested ?? '' });
        choice = answer.choice;
        if (answer.all) this.rest = answer.choice;
        continue;
      }
      throw await this.#problem(result, file, true);
    }
  }

  /**
   * The finish was sent, its answer lost, and the session is gone: it probably finished. Look
   * for the file in its folder before saying anything.
   */
  async #settleLost(outcome, cleaned) {
    const name = cleaned?.to ?? outcome.file.name;
    let after = null;
    while (outcome.parentId) {
      const query = `limit=500${after ? `&after=${encodeURIComponent(after)}` : ''}`;
      const listed = await this.#call('GET', `/api/nodes/${seg(outcome.parentId)}/children?${query}`);
      if (!listed.ok) break;
      if (listed.data.items.some((item) => item.name === name)) {
        return this.#change(outcome, { status: 'done', sent: outcome.size, name, renamed: cleaned, message: '' });
      }
      after = listed.data.next;
      if (!after) break;
    }
    throw new Stop(
      `The connection dropped while “${outcome.file.name}” was being finished, so it may have finished. Check the folder before uploading it again.`,
      false,
      { ended: true },
    );
  }

  /** The folders `path` below the queue's parent, each made or, if it exists, found. */
  async #folderFor(path) {
    let at = this.parent;
    for (let depth = 0; depth < path.length; depth += 1) {
      const key = path.slice(0, depth + 1).join('\u0000');
      if (!this.folders.has(key)) this.folders.set(key, await this.#folder(at, path[depth]));
      at = this.folders.get(key);
    }
    return at;
  }

  async #folder(parent, name) {
    const made = await this.#json('POST', `/api/nodes/${seg(parent)}/children`, { kind: 'folder', name });
    if (made.ok) return made.data.id;
    if (made.error !== 'name_taken') {
      throw new Stop(`The folder “${name}” could not be made. ${messageFor(made.error)}`);
    }
    let after = null;
    do {
      const query = `limit=500${after ? `&after=${encodeURIComponent(after)}` : ''}`;
      const listed = await this.#call('GET', `/api/nodes/${seg(parent)}/children?${query}`);
      if (!listed.ok) throw new Stop(`The folder “${name}” could not be opened. ${messageFor(listed.error)}`);
      const found = listed.data.items.find((item) => item.name === name && item.kind === 'folder');
      if (found) return found.id;
      after = listed.data.next;
    } while (after);
    throw new Stop(`“${name}” already exists here and is not a folder.`);
  }

  /** The sentence for a refused request about `file`. */
  async #problem(result, file, finishing = false) {
    switch (result.error) {
      case 'quota_exceeded': {
        const info = this.site ? await this.#call('GET', `/api/sites/${seg(this.site)}`) : null;
        return new Stop(quotaRefusalText(info?.ok ? info.data : null, file));
      }
      case 'being_edited':
        return new Stop(
          `Someone is editing “${file.name}” right now, so it was not replaced. The upload is kept: try again when they have finished.`,
          true,
          { keep: finishing },
        );
      case 'busy':
        return new Stop(messageFor('busy'), true, { keep: finishing });
      case 'too_large':
        return new Stop(`“${file.name}” is larger than the largest file this server accepts.`);
      case 'bad_request':
      case 'invalid_request':
        return new Stop(`“${file.name}” cannot be uploaded here: its name or kind is not supported.`);
      case 'name_taken':
        return new Stop(`A file named “${file.name}” is already here.`);
      case 'unknown':
        if (finishing && result.status === 409 && result.data && 'current' in result.data) {
          return new Stop(
            `The upload of “${file.name}” was discarded because someone else saved a newer version while it was being sent. ` +
              'Look at what they saved, then upload it again if yours should replace it.',
            true,
          );
        }
        return new Stop(messageFor(result.error));
      default:
        return new Stop(messageFor(result.error));
    }
  }
}

// ---- dropped and picked files (these use the page) ----

/** A File as the queue takes it: `path` the folders it is in, below where it was dropped. */
function queued(file, path = []) {
  return { name: file.name, size: file.size, path, slice: (start, end) => file.slice(start, end) };
}

/** Files picked with a file input (`webkitdirectory` gives each its relative path). */
export function filesFromPicker(list) {
  return [...list].map((file) => {
    const { folders } = splitRelative(file.webkitRelativePath || '');
    return queued(file, folders);
  });
}

/**
 * The files in a drop, folders opened. The entries are taken at once (the drop's data goes
 * away when the event ends), then read.
 */
export async function filesFromDrop(dataTransfer) {
  const entries = [...(dataTransfer.items ?? [])]
    .filter((item) => item.kind === 'file')
    .map((item) => ({ entry: item.webkitGetAsEntry?.() ?? null, file: item.getAsFile() }));
  const plain = [...(dataTransfer.files ?? [])];
  if (entries.length === 0) return plain.map((file) => queued(file));
  const found = [];
  for (const { entry, file } of entries) {
    if (entry) await walk(entry, [], found);
    else if (file) found.push(queued(file));
  }
  return found;
}

async function walk(entry, path, found) {
  if (entry.isFile) {
    const file = await new Promise((resolve, reject) => entry.file(resolve, reject));
    found.push(queued(file, path));
    return;
  }
  if (!entry.isDirectory) return;
  const inside = path.concat([entry.name]);
  const reader = entry.createReader();
  for (;;) {
    const batch = await new Promise((resolve, reject) => reader.readEntries(resolve, reject));
    if (batch.length === 0) break;
    for (const child of batch) await walk(child, inside, found);
  }
}

/** `n` bytes for the progress line: `3.0 MB of 20.0 MB`. */
export const progressText = (sent, size) => `${bytesText(sent)} of ${bytesText(size)}`;

/** One line for the whole panel: `3 of 8 uploaded, 1 skipped, 1 failed`; empty with no uploads. */
export function uploadsSummary(outcomes) {
  if (outcomes.length === 0) return '';
  const count = (status) => outcomes.filter((outcome) => outcome.status === status).length;
  const skipped = count('skipped');
  const failed = count('failed');
  let text = `${count('done')} of ${outcomes.length} uploaded`;
  if (skipped) text += `, ${skipped} skipped`;
  if (failed) text += `, ${failed} failed`;
  return text;
}

/** What a file's progress bar shows: the file read so far while it is checked, then bytes sent. */
export function progressOf(outcome) {
  if (outcome.status === 'hashing') return { value: outcome.hashed ?? 0, max: outcome.size };
  if (outcome.status === 'uploading') return { value: outcome.sent, max: outcome.size };
  return null;
}
