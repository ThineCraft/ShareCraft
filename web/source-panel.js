// Pure wording and validation for libraries connected to a GitHub repository.

const READ_ONLY = 'This library cannot be changed here.';
const LAST_UPDATE = `Files shown are from the last update. ${READ_ONLY}`;

/** Classifies the source lookup without treating a failed lookup as proof there is no source. */
export function sourceAccess(result) {
  if (!result?.ok) return result?.status === 404 ? 'none' : 'unknown';
  const source = result.data;
  if (source !== null && typeof source === 'object' && source.kind === 's3') {
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

/** The S3 headline, supporting sentence and tone for an S3 library; the budget pause wins over the state. */
function s3Summary(source, now) {
  const state = source.state;
  if (source.paused) {
    return {
      headline: 'This library has used its request budget for the month. Files opened before still open.',
      detail: LAST_UPDATE,
      tone: 'attention',
    };
  }
  if (state === 'ok') {
    const age = relativeTime(source.last_checked_at, now);
    return {
      headline: 'Shows the files in S3 storage.',
      detail: `${age ? `Checked ${age}. ` : ''}Files open from the bucket when you ask for them; this library cannot be changed here.`,
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
        ? `Listing the bucket… ${files.toLocaleString('en-GB')} files so far`
        : 'Listing the bucket…',
      detail: LAST_UPDATE,
      tone: 'working',
    };
  }
  if (state === 'not_connected') {
    return { headline: 'The storage refused the access key.', detail: LAST_UPDATE, tone: 'attention' };
  }
  if (state === 'failed' || state === 'rate_limited') {
    return {
      headline: 'The storage could not be reached. Files already opened still open.',
      detail: LAST_UPDATE,
      tone: 'attention',
    };
  }
  return { headline: sourceStateText(state), detail: LAST_UPDATE, tone: 'attention' };
}

/** The headline, supporting sentence and non-colour tone for a connected-library banner. */
export function sourceSummary(source, now = Date.now()) {
  const state = source?.state;
  if (source?.kind === 's3') return s3Summary(source, now);
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

/** The text mark for a file the listing reports as unavailable; never colour alone. */
export function unavailableText(reason) {
  return Object.hasOwn(UNAVAILABLE_TEXT, reason) ? UNAVAILABLE_TEXT[reason] : 'Unavailable';
}

/** The text mark for a listed item the server reports as unavailable (`unavailable`), or ''. */
export function unavailableMarkFor(item) {
  if (item?.unavailable === undefined || item?.unavailable === null || item.unavailable === '') return '';
  return unavailableText(item.unavailable);
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
