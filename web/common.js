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
  too_large: 'That file is too large.',
  unknown_editor: 'That kind of document cannot be made here.',
  upload_required: 'A PDF cannot be made empty. Upload a PDF file instead.',
  wrong_extension: 'That name does not end with the extension of this kind of document.',
};

/** What a document page says when the document is missing or the person has no access. */
export const MISSING_DOCUMENT = 'This document does not exist, or you do not have access to it.';

const REMOVED = Object.assign(Object.create(null), {
  access_revoked: 'Your access to this document was removed.',
  signed_out: 'You were signed out.',
  account_disabled: 'Your account was disabled.',
});

/** What a document page says when the server removed it, for the server's reason. */
export function removedMessage(reason) {
  return REMOVED[reason] ?? 'Your access to this document ended.';
}

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
 * The page to open after signing in: `next` only if it is a path on this site, otherwise the
 * home page. It must start with one slash (not followed by another slash or a backslash),
 * hold no control or space characters (browsers drop tabs and line breaks, which can turn
 * "/" + tab + "/host" into "//host"), and resolve to `origin`. Pure: `origin` is passed in.
 */
export function safeNext(next, origin = globalThis.location?.origin) {
  if (typeof next !== 'string' || !next.startsWith('/')) return '/';
  if (next[1] === '/' || next[1] === '\\') return '/';
  for (const ch of next) {
    const code = ch.codePointAt(0);
    if (code <= 0x20 || code === 0x7f) return '/';
  }
  try {
    const url = new URL(next, origin);
    if (url.origin !== origin) return '/';
    return url.pathname + url.search + url.hash;
  } catch {
    return '/';
  }
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

/** Puts `text` in a message paragraph, which stays in the page (empty) so readers announce changes. */
export function showMessage(element, text) {
  element.textContent = text;
}

/**
 * The Sharing panel of document `docId`, used by every document page. The page holds the
 * elements `access`, `share-form`, `share-email`, `share-role` and `share-status`, and decides
 * where and when the panel shows. Returns:
 * - `setRole(role)`: loads the access list for an owner; returns whether the person is one.
 * - `stop()`: empties the panel for good (the page was removed); late answers are dropped.
 */
export function sharing(docId) {
  const $ = (id) => document.getElementById(id);
  let stopped = false;
  let accessCheck = 0; // the latest access-list request; older answers are dropped

  async function showAccess() {
    const check = ++accessCheck;
    const result = await api('GET', `/docs/${seg(docId)}/access`);
    if (check !== accessCheck || stopped) return;
    if (!result.ok) {
      shareMessage(messageFor(result.error), true);
      return;
    }
    $('access').replaceChildren(
      ...result.data.map(({ user, role }) => {
        const select = h('select', { 'aria-label': `Role of ${user.name}` });
        for (const value of ['viewer', 'editor', 'owner']) {
          const option = h('option', { value }, value[0].toUpperCase() + value.slice(1));
          option.selected = value === role;
          select.append(option);
        }
        select.addEventListener('change', () => changeAccess(user.email, select.value));
        const remove = h(
          'button',
          { type: 'button', class: 'plain', 'aria-label': `Remove ${user.name}` },
          'Remove',
        );
        remove.addEventListener('click', () => removeAccess(user));
        return h(
          'li',
          {},
          h('span', { class: 'grow' }, user.name, h('br'), h('small', {}, user.email)),
          select,
          remove,
        );
      }),
    );
  }

  async function changeAccess(email, role) {
    shareMessage('', false);
    const result = await api('PUT', `/docs/${seg(docId)}/access`, { email, role });
    if (stopped) return;
    if (!result.ok) shareMessage(messageFor(result.error), true);
    else shareMessage('Saved.', false);
    await showAccess();
  }

  async function removeAccess(user) {
    shareMessage('', false);
    const result = await api('DELETE', `/docs/${seg(docId)}/access/${seg(user.id)}`);
    if (stopped) return;
    if (!result.ok) shareMessage(messageFor(result.error), true);
    else shareMessage(`${user.name} was removed.`, false);
    await showAccess();
  }

  $('share-form').addEventListener('submit', async (event) => {
    event.preventDefault();
    const email = $('share-email').value.trim();
    const result = await api('PUT', `/docs/${seg(docId)}/access`, {
      email,
      role: $('share-role').value,
    });
    if (stopped) return;
    if (result.ok) {
      $('share-email').value = '';
      shareMessage('Shared.', false);
    } else {
      shareMessage(messageFor(result.error), true);
    }
    await showAccess();
  });

  function shareMessage(text, isError) {
    const line = $('share-status');
    line.classList.toggle('ok', !isError);
    showMessage(line, text);
  }

  return {
    setRole(role) {
      const owner = role === 'owner' && !stopped;
      if (owner) showAccess();
      return owner;
    },
    stop() {
      stopped = true;
      accessCheck++;
      $('access').replaceChildren();
      $('share-email').value = '';
      shareMessage('', false);
    },
  };
}
