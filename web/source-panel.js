// Pure wording and validation for libraries connected to a GitHub repository.

const READ_ONLY = 'This library cannot be changed here.';
const LAST_UPDATE = `Files shown are from the last update. ${READ_ONLY}`;

/** Classifies the source lookup without treating a failed lookup as proof there is no source. */
export function sourceAccess(result) {
  if (!result?.ok) return result?.status === 404 ? 'none' : 'unknown';
  const source = result.data;
  return source !== null && typeof source === 'object' && typeof source.repository === 'string' &&
      typeof source.branch === 'string' && typeof source.state === 'string'
    ? 'connected'
    : 'unknown';
}

/** Source-managed libraries and uncertain source lookups are both read-only. */
export function sourceControlsEnabled(access) {
  return access === 'none';
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

/** The headline, supporting sentence and non-colour tone for a connected-library banner. */
export function sourceSummary(source, now = Date.now()) {
  const state = source?.state;
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

/** The four values to copy into GitHub's Add webhook form. */
export function webhookInstructions(webhook, secret) {
  return [
    `Payload URL: ${webhook?.url ?? ''}`,
    'Content type: application/json',
    `Secret: ${secret ?? ''}`,
    'Events: Just the push event',
  ];
}
