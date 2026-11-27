// Pure wording and validation for libraries connected to a GitHub repository or an S3 bucket.

const READ_ONLY = 'This library cannot be changed here.';
const LAST_UPDATE = `Files shown are from the last update. ${READ_ONLY}`;

/** Classifies the source lookup without treating a failed lookup as proof there is no source. */
export function sourceAccess(result) {
  if (!result?.ok) return result?.status === 404 ? 'none' : 'unknown';
  const source = result.data;
  if (source !== null && typeof source === 'object' && (source.kind === 's3' || source.kind === 'share')) {
    return typeof source.state === 'string' ? 'connected' : 'unknown';
  }
  return source !== null && typeof source === 'object' && typeof source.repository === 'string' &&
      typeof source.branch === 'string' && typeof source.state === 'string'
    ? 'connected'
    : 'unknown';
}

/** Source-managed libraries and uncertain source lookups are both read-only. */
export function sourceControlsEnabled(access) {
  return access === 'none';
}

const NO_ACTIONS = { add: false, folder: false, rename: false, move: false, remove: false, copy: false };

/** Whether a connected S3 library accepts changes now: read and write, and not blocked. */
export function s3Writable(source) {
  return source?.kind === 's3' && source.mode === 'read_write' && source.write_state !== 'blocked';
}

/**
 * What a person may do here, as far as the library's source goes: everything in a library with
 * no source; everything a bucket can do in a writable, unblocked S3 library (moving stays inside
 * the library, see moveStaysInLibrary); nothing otherwise.
 * (The server decides who may add; this only leaves out what it would refuse.)
 */
export function sourceActions(access, source) {
  if (access === 'none') return { add: true, folder: true, rename: true, move: true, remove: true, copy: true };
  if (access === 'connected' && s3Writable(source)) return { add: true, folder: true, rename: true, move: true, remove: true, copy: true };
  return { ...NO_ACTIONS };
}

/** Whether a refreshed source status means the visible folder may have changed. */
export function shouldReload(previous, next) {
  if (previous === null || typeof previous !== 'object' || next === null || typeof next !== 'object') return false;
  if ((previous.last_commit ?? null) !== (next.last_commit ?? null)) return true;
  return ['connecting', 'syncing'].includes(previous.state) && next.state === 'ok';
}

/** The short, visible label for a source state. */
export function sourceStateText(state) {
  return {
    ok: 'Up to date',
    connecting: 'Connecting…',
    syncing: 'Updating…',
    rate_limited: 'Waiting for GitHub (rate limit)',
    not_connected: 'Not connected',
    failed: 'Update failed',
  }[state] ?? 'Source status unavailable';
}

/** An RFC 3339 time as a compact age in seconds, minutes, hours or days. */
export function relativeTime(iso, now = Date.now()) {
  if (iso == null || iso === '') return '';
  const at = new Date(iso).getTime();
  const current = new Date(now).getTime();
  if (!Number.isFinite(at) || !Number.isFinite(current)) return '';
  const seconds = Math.max(0, Math.floor((current - at) / 1000));
  if (seconds < 5) return 'just now';
  if (seconds < 60) return `${seconds} ${seconds === 1 ? 'second' : 'seconds'} ago`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes} ${minutes === 1 ? 'minute' : 'minutes'} ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours} ${hours === 1 ? 'hour' : 'hours'} ago`;
  const days = Math.floor(hours / 24);
  return `${days} ${days === 1 ? 'day' : 'days'} ago`;
}

const sentence = (text) => {
  const clean = typeof text === 'string' ? text.trim() : '';
  return clean === '' || /[.!?] ?$/.test(clean) ? clean : `${clean}.`;
};

/** The reset clock embedded in the server-owned rate-limit sentence, when it is present. */
function rateLimitTime(error) {
  const match = typeof error === 'string' ? /\bafter\s+\d{4}-\d\d-\d\d[T ](\d\d:\d\d)/i.exec(error) : null;
  return match?.[1] ?? '';
}

const NOT_KNOWN = 'Nothing more is known. Try again from the library settings.';
const blockedDetail = (source) => sentence(source?.write_error) || NOT_KNOWN;

/** The settings page's words for a blocked library: the headline once, then the stored reason. */
export function blockedText(source) {
  return `Read-only until fixed. ${blockedDetail(source)}`;
}

/** The S3 headline, supporting sentence and tone for an S3 library; the budget pause wins over the state. */
function s3Summary(source, now) {
  const state = source.state;
  const writable = source.mode === 'read_write';
  const blocked = writable && source.write_state === 'blocked';
  // A library that takes changes does not say it cannot be changed.
  const lastUpdate = writable ? 'Files shown are from the last update.' : LAST_UPDATE;
  const readOnlyEnd = writable ? '' : ` ${READ_ONLY}`;
  if (source.paused) {
    return {
      headline: 'This library has used its request budget for the month. Files opened before still open.',
      detail: lastUpdate,
      tone: 'attention',
    };
  }
  if (blocked) {
    return {
      headline: 'Read-only until fixed.',
      detail: blockedDetail(source),
      tone: 'attention',
    };
  }
  if (state === 'ok') {
    const age = relativeTime(source.last_checked_at, now);
    if (writable) {
      return {
        headline: 'Stored in S3 storage.',
        detail: `${age ? `Checked ${age}. ` : ''}Changes you make here are saved to the bucket.`,
        tone: 'ok',
      };
    }
    return {
      headline: 'Shows the files in S3 storage.',
      detail: `${age ? `Checked ${age}. ` : ''}Files open from the bucket when you ask for them; this library cannot be changed here.`,
      tone: 'ok',
    };
  }
  if (state === 'connecting') {
    return {
      headline: sourceStateText(state),
      detail: `Files will appear after the first listing.${readOnlyEnd}`,
      tone: 'working',
    };
  }
  if (state === 'syncing') {
    const files = Number(source.listing_now?.files_so_far);
    return {
      headline: Number.isFinite(files) && files >= 0
        ? `Listing the bucket… ${files.toLocaleString('en-GB')} files so far`
        : 'Listing the bucket…',
      detail: lastUpdate,
      tone: 'working',
    };
  }
  if (state === 'not_connected') {
    return { headline: 'The storage refused the access key.', detail: lastUpdate, tone: 'attention' };
  }
  if (state === 'failed' || state === 'rate_limited') {
    return {
      headline: 'The storage could not be reached. Files already opened still open.',
      detail: lastUpdate,
      tone: 'attention',
    };
  }
  return { headline: sourceStateText(state), detail: lastUpdate, tone: 'attention' };
}

const SHARE_UNREACHABLE = 'The share could not be reached. Files already opened still open; others will not until it is back.';
const SHARE_NOT_APPROVED = 'This folder is no longer approved, so its files cannot be opened.';

/** The file-share headline, supporting sentence and tone; the share is read-only in this release. */
function shareSummary(source, now) {
  const state = source.state;
  if (state === 'ok') {
    const age = relativeTime(source.last_checked_at, now);
    return {
      headline: `Shows the files in ${sourceName(source)}.`,
      detail: `${age ? `Checked ${age}. ` : ''}Files open from the share when you ask for them; this library cannot be changed here.`,
      tone: 'ok',
    };
  }
  if (state === 'connecting') {
    return {
      headline: sourceStateText(state),
      detail: `Files will appear after the first listing. ${READ_ONLY}`,
      tone: 'working',
    };
  }
  if (state === 'syncing') {
    const files = Number(source.listing_now?.files_so_far);
    return {
      headline: Number.isFinite(files) && files >= 0
        ? `Listing the share… ${files.toLocaleString('en-GB')} files so far`
        : 'Listing the share…',
      detail: LAST_UPDATE,
      tone: 'working',
    };
  }
  if (state === 'not_connected') return { headline: SHARE_NOT_APPROVED, detail: LAST_UPDATE, tone: 'attention' };
  if (state === 'failed') {
    // The worker's own sentence says why a listing was refused (over its limits, not allowed to
    // read the folder); an unreachable share has the fixed sentence.
    const why = sentence(source.last_error);
    const unreachable = why === '' || /^The share could not be reached/.test(why);
    return { headline: unreachable ? SHARE_UNREACHABLE : why, detail: LAST_UPDATE, tone: 'attention' };
  }
  return { headline: SHARE_UNREACHABLE, detail: LAST_UPDATE, tone: 'attention' };
}

/** The headline, supporting sentence and non-colour tone for a connected-library banner. */
export function sourceSummary(source, now = Date.now()) {
  const state = source?.state;
  if (source?.kind === 's3') return s3Summary(source, now);
  if (source?.kind === 'share') return shareSummary(source, now);
  if (state === 'ok') {
    const age = relativeTime(source.last_synced_at, now);
    return {
      headline: `Kept in step with github.com/${source.repository} (branch ${source.branch}).`,
      detail: `${age ? `Updated ${age}. ` : ''}${READ_ONLY}`,
      tone: 'ok',
    };
  }
  if (state === 'connecting') {
    return {
      headline: sourceStateText(state),
      detail: `Files will appear after the first update. ${READ_ONLY}`,
      tone: 'working',
    };
  }
  if (state === 'syncing') return { headline: sourceStateText(state), detail: LAST_UPDATE, tone: 'working' };
  if (state === 'rate_limited') {
    const until = rateLimitTime(source.last_error);
    return {
      headline: `${sourceStateText(state)}${until ? ` until ${until}` : ''}`,
      detail: LAST_UPDATE,
      tone: 'attention',
    };
  }
  if (state === 'not_connected' || state === 'failed') {
    const reason = sentence(source.last_error);
    return {
      headline: `${sourceStateText(state)}${reason ? `: ${reason}` : '.'}`,
      detail: LAST_UPDATE,
      tone: 'attention',
    };
  }
  return { headline: sourceStateText(state), detail: LAST_UPDATE, tone: 'attention' };
}

/** Whether `text` is a GitHub repository named as `owner/repo`, not an address. */
export function validRepository(text) {
  if (typeof text !== 'string' || text !== text.trim()) return false;
  return /^[A-Za-z0-9](?:[A-Za-z0-9-]{0,38})\/[A-Za-z0-9._-]+$/.test(text);
}

/** The name of a connected source for its owners: `github.com/owner/repo`, or `bucket/folder` for S3. */
export function sourceName(source) {
  if (source?.kind === 'share') {
    // The path is the server's to give (owners only); without one, never a guess.
    return typeof source.path === 'string' && source.path !== '' ? source.path : 'a file share';
  }
  if (source?.kind === 's3') {
    if (typeof source.bucket !== 'string' || source.bucket === '') return 'S3 storage';
    const folder = typeof source.prefix === 'string' ? source.prefix.replace(/\/+$/, '') : '';
    return folder === '' ? source.bucket : `${source.bucket}/${folder}`;
  }
  return typeof source?.repository === 'string' ? `github.com/${source.repository}` : '';
}

const UNAVAILABLE_TEXT = {
  archived: 'In archive storage',
  gone: 'Not in the bucket',
  denied: 'Access refused',
  budget: 'Over the request budget',
};

const SHARE_UNAVAILABLE_TEXT = {
  gone: 'Not on the share',
  denied: 'Access refused',
  offline: 'Share not reachable',
  changing: 'Changing, try again',
  too_large: 'Too large to open',
  not_approved: 'Folder not approved',
};

/**
 * The text mark for a file the listing reports as unavailable; never colour alone. `kind` is the
 * source's kind: `share` for a file share; anything else keeps the bucket wording.
 */
export function unavailableText(reason, kind) {
  if (kind === 'share' && Object.hasOwn(SHARE_UNAVAILABLE_TEXT, reason)) return SHARE_UNAVAILABLE_TEXT[reason];
  if (kind !== 'share' && Object.hasOwn(UNAVAILABLE_TEXT, reason)) return UNAVAILABLE_TEXT[reason];
  if (kind === 'share' && (reason === 'archived' || reason === 'budget')) return UNAVAILABLE_TEXT[reason];
  return 'Unavailable';
}

/** The text mark for a listed item the server reports as unavailable (`unavailable`), or ''. */
export function unavailableMarkFor(item, kind) {
  if (item?.unavailable === undefined || item?.unavailable === null || item.unavailable === '') return '';
  return unavailableText(item.unavailable, kind);
}

/** A first check of the folder path of a file share; the server decides. */
export function validSharePath(text) {
  const refuse = (reason) => ({ ok: false, reason });
  if (typeof text !== 'string' || text.trim() === '') return refuse('Write the folder as a network path.');
  const path = text.trim();
  if (path.length > 1024) return refuse('Use at most 1,024 characters.');
  if (/[\u0000-\u001f\u007f]/.test(path)) return refuse('Do not use control characters.');
  if (/^[\\/]{2}[?.][\\/]/.test(path)) return refuse('Device paths cannot be used as file shares.');
  let names;
  if (/^[\\/]{2}/.test(path)) {
    names = path.slice(2).split(/[\\/]/);
    if (names.length < 2 || names[0] === '' || names[1] === '') {
      return refuse('Write the server and the share, like \\\\server\\share\\folder.');
    }
  } else if (/^[A-Za-z]:[\\/]/.test(path)) {
    names = path.slice(3).split(/[\\/]/);
    if (names.every((name) => name === '')) return refuse('Choose a folder, not a whole drive.');
  } else if (path.startsWith('/')) {
    names = path.slice(1).split(/[\\/]/);
  } else {
    return refuse('Write the full path, starting with \\\\server\\share or a drive letter.');
  }
  // One trailing separator is fine; an empty name anywhere else is not.
  if (names.length > 1 && names.at(-1) === '') names = names.slice(0, -1);
  if (names.some((name) => name === '')) return refuse('Do not leave an empty folder name between separators.');
  if (names.some((name) => name === '.' || name === '..')) return refuse('Do not use . or .. in the path.');
  if (names.some((name) => /[<>"|?*:]/.test(name))) return refuse('A folder name cannot hold any of < > " | ? * :');
  return { ok: true, reason: '' };
}

const HIDDEN_WORDS = [
  ['links', 'link to another place, not followed', 'links to other places, not followed'],
  ['submodules', 'submodule', 'submodules'],
  ['system', 'hidden or system file', 'hidden or system files'],
  ['offline', 'cloud-only file that is not on the share itself', 'cloud-only files that are not on the share itself'],
  ['names', 'file with a name ShareCraft cannot use', 'files with names ShareCraft cannot use'],
  ['types', 'file of a type this library does not take', 'files of types this library does not take'],
  ['size', 'file that is too large', 'files that are too large'],
  ['long', 'file with a path that is too long', 'files with paths that are too long'],
  ['unreadable', 'folder the server may not read', 'folders the server may not read'],
];

/** One plain line for each kind of hidden item the server counted; kinds with a zero count are left out. */
export function hiddenLines(hiddenBy) {
  if (hiddenBy === null || typeof hiddenBy !== 'object') return [];
  const lines = [];
  for (const [key, one, many] of HIDDEN_WORDS) {
    const count = Number(hiddenBy[key]);
    if (Number.isFinite(count) && count > 0) lines.push(`${count.toLocaleString('en-GB')} ${count === 1 ? one : many}`);
  }
  return lines;
}

/** How many times a page asks again while the share is being read, and the wait between asks. */
export const SHARE_RETRIES = 5;
export const SHARE_RETRY_MS = 2000;
export const FETCHING_TEXT = 'Fetching from the share…';

/**
 * Whether an answer to opening a file means "the share is being read: ask again": the server is
 * busy (503 `busy`) or the content waits for the scanner after it was fetched (423 with
 * `scan: pending`). A held or infected file is not this.
 */
export function fetchingFromShare(status, data) {
  if (status === 503) return data?.error === 'busy';
  return status === 423 && data?.scan === 'pending';
}

/** The words for a refused open of a file that lives outside ShareCraft, or '' for any other answer. */
export function openRefusedText(status, data) {
  if (status !== 409 || data === null || typeof data !== 'object') return '';
  const message = typeof data.message === 'string' ? data.message.trim() : '';
  if (data.error === 'source_unavailable') return message || `${unavailableText(data.reason, 'share')}.`;
  if (data.error === 'source_copy_unavailable') return message;
  return '';
}

/** A first check of an S3 bucket name; the server decides. */
export function validBucket(text) {
  const refuse = (reason) => ({ ok: false, reason });
  if (typeof text !== 'string' || text.length < 3 || text.length > 63) {
    return refuse('Use 3 to 63 characters.');
  }
  if (!/^[a-z0-9.-]+$/.test(text)) return refuse('Use only lower-case letters, digits, dots and hyphens.');
  if (!/^[a-z0-9].*[a-z0-9]$/.test(text)) return refuse('Start and end with a letter or digit.');
  if (text.includes('..')) return refuse('Do not use two dots in a row.');
  if (/^\d{1,3}(\.\d{1,3}){3}$/.test(text)) return refuse('Write the bucket name, not an IP address.');
  return { ok: true, reason: '' };
}

/** A first check of the folder in a bucket; empty means the whole bucket. */
export function validPrefix(text) {
  if (typeof text !== 'string') return { ok: false, reason: 'Write the folder as text.' };
  if (text === '') return { ok: true, reason: '' };
  if (text.length > 512) return { ok: false, reason: 'Use at most 512 characters.' };
  if (text.startsWith('/')) return { ok: false, reason: 'Do not start the folder with a slash.' };
  if (/[\u0000-\u001f\u007f]/.test(text)) return { ok: false, reason: 'Do not use control characters.' };
  if (text.split('/').some((segment) => segment === '.' || segment === '..')) {
    return { ok: false, reason: 'Do not use . or .. as a folder name.' };
  }
  return { ok: true, reason: '' };
}

/** The four values to copy into GitHub's Add webhook form. */
export function webhookInstructions(webhook, secret) {
  return [
    `Payload URL: ${webhook?.url ?? ''}`,
    'Content type: application/json',
    `Secret: ${secret ?? ''}`,
    'Events: Just the push event',
  ];
}

// ---- writing to a bucket ----

const plural = (n, word) => `${n} ${word}${n === 1 ? '' : 's'}`;

/** "Saving 3 files…" and "2 files could not be saved." from the source's pending counts, or ''. */
export function writeCountsText(writes) {
  const saving = Number(writes?.saving) > 0 ? Number(writes.saving) : 0;
  const failed = Number(writes?.failed) > 0 ? Number(writes.failed) : 0;
  return [
    saving > 0 ? `Saving ${plural(saving, 'file')}…` : '',
    failed > 0 ? `${plural(failed, 'file')} could not be saved.` : '',
  ]
    .filter(Boolean)
    .join(' ');
}

/** The reason shown on a failed row when the listing has none of its own. */
const FAILED_REASON = 'it could not be written to the bucket.';

/** The text mark under a name for a change not yet written: "Saving…", "Not saved: <reason>", or ''. */
export function savingMark(item, reason = '') {
  if (item?.saving === 'saving') return 'Saving…';
  if (item?.saving === 'failed') {
    const why = typeof reason === 'string' && reason.trim() !== '' ? reason.trim() : FAILED_REASON;
    return `Not saved: ${why}`;
  }
  return '';
}

/** What Discard asks first; a file that was never in the bucket is deleted by it. */
export function discardQuestion(isNew) {
  return isNew
    ? 'Discard this file? It was never saved to the bucket.'
    : 'Discard this change? The file goes back to what is in the bucket.';
}

/** The sentence for a change this kind of library does not take yet. */
export function sourceActionUnavailableText() {
  return 'This cannot be done yet in a library stored in S3.';
}

/** A limit in the largest unit that holds it exactly, as the server says it: "5 GiB", "512 MiB". */
function limitWords(limit) {
  for (const [unit, size] of [['GiB', 1024 ** 3], ['MiB', 1024 ** 2], ['KiB', 1024]]) {
    if (limit >= size && limit % size === 0) return `${limit / size} ${unit}`;
  }
  return `${limit} bytes`;
}

/** The sentence for a file over the single-upload limit; `limit` in bytes, when the server said. */
export function sourceFileTooLargeText(limit) {
  if (typeof limit !== 'number' || !Number.isFinite(limit) || limit <= 0) {
    return 'This file is larger than a library stored in S3 takes. Nothing was stored.';
  }
  return `Files over ${limitWords(limit)} cannot be saved to this S3 storage.`;
}

/** The sentence for a change refused because the library is read-only until fixed. */
export function sourceWriteBlockedText() {
  return 'This library is read-only until the storage accepts changes again. An owner can try again from the library settings.';
}

/** The sentence for a non-administrator asking for read and write. */
export function readWriteAdminOnlyText() {
  return 'Only an administrator can make a library read and write.';
}

const WRITE_TEXTS = {
  s3_not_writable:
    'That access key can read the bucket but is not allowed to write to it. Use a key that may write, or connect read-only.',
  source_writes_pending:
    'Some changes are still being saved to the bucket. Wait until they are saved or discarded, then try again.',
};

// Said when the server sent no message of its own; the server's message is shown when it did.
const SERVER_WORDED = {
  source_item_saving:
    'This is still being saved to the storage. Try again when saving has finished, or discard the save that failed.',
  source_too_large_to_move:
    'Files over 5 GiB in S3 storage cannot be renamed, moved, copied or deleted from ShareCraft yet.',
  source_changed:
    'This changed in the storage since ShareCraft last looked. ShareCraft is checking it again; try again in a minute.',
  source_copy_incomplete:
    'The copy stopped part way: some items were copied and are being saved to the storage; the rest were not copied.',
};

const serverMessage = (data) => (typeof data?.message === 'string' ? data.message.trim() : '');

/** "Done. 3 of the original objects could not be removed…" for an answer carrying `left_behind`, else ''. */
export function leftBehindText(data) {
  const n = Number(data?.left_behind);
  if (!Number.isFinite(n) || n <= 0) return '';
  return `Done. ${n.toLocaleString('en-GB')} of the original objects could not be removed from the bucket; they will show up as separate files until someone removes them.`;
}

/** Whether a move from this library's source stays in the library; a bucket cannot move between libraries. */
export function moveStaysInLibrary(source, libraryId, trail) {
  if (source?.kind !== 's3') return true;
  return (trail ?? []).some((step) => step.id === libraryId);
}

/** Why a move out of a bucket library is not offered. */
export function moveAcrossLibrariesText() {
  return 'Items in a library stored in S3 cannot be moved between libraries yet. Copy them instead, or choose a folder in this library.';
}

/**
 * The revision an editor's bytes are read at. A file on a share has no stored version, so its
 * session announces none: the revision the file's own details give (the share's tag) is used.
 * A normal document that was never saved stays null and starts empty.
 */
export function contentRevision(sessionRevision, doc) {
  if (sessionRevision) return sessionRevision;
  return doc?.external && doc.revision ? doc.revision : null;
}

/** The sentence for a refusal that only connecting a file-share folder gives, or '' for any other code. */
export function shareConnectErrorText(code) {
  return code === 'share_location_in_use'
    ? 'Another library already shows this folder, or a folder inside or around it.'
    : '';
}

/** The sentence for an error code of writing to a bucket, or '' for any other code. */
export function sourceWriteErrorText(code, data) {
  const message = serverMessage(data);
  if (code === 'left_behind') return leftBehindText(data);
  if (code === 'source_unavailable') return data?.reason === 'archived' ? message : '';
  if (code === 'source_too_many_objects') {
    if (message) return message;
    const count = Number(data?.count);
    return `This folder holds ${Number.isFinite(count) && count > 0 ? count.toLocaleString('en-GB') : 'more than 1,000'} objects in the bucket. ShareCraft renames, moves, copies or deletes at most 1,000 at a time.`;
  }
  if (Object.hasOwn(SERVER_WORDED, code)) return message || SERVER_WORDED[code];
  if (code === 'source_action_unavailable') return message || sourceActionUnavailableText();
  if (code === 'source_file_too_large') return message || sourceFileTooLargeText(Number(data?.limit));
  if (code === 'source_write_blocked') return sourceWriteBlockedText();
  if (code === 'read_write_admin_only') return readWriteAdminOnlyText();
  return Object.hasOwn(WRITE_TEXTS, code) ? WRITE_TEXTS[code] : '';
}

/**
 * The one mode change the settings page offers, or null: administrators make a library read and
 * write (a blocked one is tried again the same way); owners switch a writable library back.
 */
export function modeSwitchText(source, isAdmin) {
  if (source?.kind !== 's3') return null;
  if (source.mode === 'read_write') {
    if (source.write_state === 'blocked' && isAdmin) return { to: 'read_write', label: 'Try again' };
    return { to: 'read_only', label: 'Switch to read-only' };
  }
  return isAdmin ? { to: 'read_write', label: 'Switch to read and write' } : null;
}

/** What the storage does with a save that would overwrite an outside change; '' when not known. */
export function overwriteNote(conditional) {
  if (conditional === 'yes') return "This storage refuses a save that would overwrite someone else's change.";
  if (conditional === 'no' || conditional === 'unknown') {
    return 'This storage cannot refuse a conflicting save. A change made in the bucket in the same instant as a save here can be overwritten.';
  }
  return '';
}

/** Whether the server offered this row or file's pending change to the person to Retry or Discard. */
export function mayResolve(entry) {
  return entry?.may_resolve === true;
}
