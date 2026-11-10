// Shared by every page: calling the API, turning its error codes into sentences, and the
// sign-in redirect. Pages hold no merging or permission logic; the server decides.

const MESSAGES = {
  invalid_credentials: 'That email and password do not match.',
  too_many_attempts: 'Too many attempts. Try again in a few minutes.',
  not_signed_in: 'You are not signed in.',
  no_such_user: 'No account has that email address.',
  last_owner: 'A document must keep at least one owner.',
  last_admin: 'At least one administrator must stay enabled.',
  email_taken: 'An account with that email address already exists.',
  invalid_email: 'That is not a usable email address.',
  invalid_name: 'That name is not allowed. Use 1 to 60 plain characters.',
  weak_password: 'That password is too short or too long.',
  not_found: 'That could not be found.',
  forbidden: 'That request was refused. Reload the page and try again.',
  network: 'Could not reach the server. Check the connection and try again.',
};

/** The sentence for an API error code; unknown codes get a general one. */
export function messageFor(code) {
  return Object.hasOwn(MESSAGES, code) ? MESSAGES[code] : 'Something went wrong. Try again.';
}

/**
 * Calls `/api{path}`. Never throws: returns { ok, status, data, error } where `error` is the
 * server's error code (or "network"). A 401 "not_signed_in" sends the page to the sign-in page.
 */
export async function api(method, path, body) {
  const init = { method, credentials: 'same-origin', headers: {} };
  if (body !== undefined) {
    init.headers['Content-Type'] = 'application/json';
    init.body = JSON.stringify(body);
  }
  let response;
  try {
    response = await fetch(`/api${path}`, init);
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

/** Path segment for an id taken from the page or the server. */
export const seg = (value) => encodeURIComponent(value);

/** Opens the sign-in page, which brings the person back to this page and hash afterwards. */
export function goToSignIn() {
  const next = location.pathname + location.search + location.hash;
  location.replace(`/signin.html?next=${encodeURIComponent(next)}`);
}

/**
 * The page to open after signing in: `next` only if it is a path on this site (one leading
 * slash, not followed by another slash or a backslash), otherwise the home page.
 */
export function safeNext(next) {
  if (typeof next === 'string' && next.startsWith('/') && next[1] !== '/' && next[1] !== '\\') {
    return next;
  }
  return '/';
}

/**
 * The signed-in account, from `GET /api/me`. Without a session the page goes to the sign-in
 * page and this never returns.
 */
export async function requireMe() {
  const result = await api('GET', '/me');
  if (result.ok) return result.data;
  if (result.status === 401) await new Promise(() => {}); // already on its way to sign in
  throw new Error(messageFor(result.error));
}

/** Builds an element. Children are strings (put in as text) or nodes. */
export function h(tag, attributes = {}, ...children) {
  const element = document.createElement(tag);
  for (const [name, value] of Object.entries(attributes)) {
    if (value === false || value == null) continue;
    element.setAttribute(name, value === true ? '' : String(value));
  }
  element.append(...children);
  return element;
}

/** Shows `text` in the error paragraph `element`, or hides it when `text` is empty. */
export function showMessage(element, text) {
  element.textContent = text;
  element.hidden = text === '';
}
