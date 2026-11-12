// Shared by every page: calling the API, turning its error codes into sentences, and the
// sign-in redirect. Pages hold no merging or permission logic; the server decides.

const MESSAGES = {
  invalid_credentials: 'That email and password do not match.',
  too_many_attempts: 'Too many attempts. Try again in a few minutes.',
  not_signed_in: 'You are not signed in.',
  no_such_user: 'No account has that email address.',
  last_owner: 'There must be at least one owner. Make someone else an owner first.',
  last_manager:
    'Someone must still be able to manage access here. Give someone else that first.',
  beyond_your_access: 'You cannot give a role with permissions you do not have here.',
  last_admin: 'At least one administrator must stay enabled.',
  email_taken: 'An account with that email address already exists.',
  invalid_email: 'That is not a usable email address.',
  invalid_name:
    'That name is not allowed. Use 1 to 120 characters, without / \\ : * ? " < > | and not ending in a dot.',
  weak_password: 'That password is too short or too long.',
  not_found: 'That could not be found.',
  forbidden: 'That request was refused. Reload the page and try again.',
  network: 'Could not reach the server. Check the connection and try again.',
  too_large: 'That file is too large.',
  unknown_editor: 'That kind of document cannot be made here.',
  upload_required: 'A PDF cannot be made empty. Upload a PDF file instead.',
  wrong_extension: 'That name does not end with the extension of this kind of document.',
  name_taken: 'Something here already has that name. Choose a different name.',
  not_empty: 'That folder still has items in it. Move or delete them first.',
  into_itself: 'A folder cannot be moved into itself or into a folder inside it.',
  wrong_place: 'That cannot go there. Choose a library or a folder.',
  is_site: 'A site cannot be moved, deleted or made to inherit.',
  invalid_request: 'That change is not allowed. A file keeps its extension when renamed.',
  unavailable: 'The server is busy or its database is unavailable. Try again in a moment.',
  bad_request: 'The server could not read that request. Reload the page and try again.',
  not_json: 'The server could not read that request. Reload the page and try again.',
};

/** What a document page says when the document is missing or the person has no access. */
export const MISSING_DOCUMENT = 'This document does not exist, or you do not have access to it.';

const REMOVED = Object.assign(Object.create(null), {
  access_revoked: 'Your access to this document was removed.',
  signed_out: 'You were signed out.',
  account_disabled: 'Your account was disabled.',
  deleted: 'This document was deleted.',
});

/** What a document page says when the server removed it, for the server's reason. */
export function removedMessage(reason) {
  return REMOVED[reason] ?? 'Your access to this document ended.';
}

/**
 * The reason in a `removed` frame from the server, read without the text client: a frame the
 * client could not read must still end the page as a removal, not as a broken connection.
 * `'unknown'` when the reason is missing; `null` unless `data` is a `removed` frame.
 */
export function removedReasonOf(data) {
  let message;
  try {
    message = JSON.parse(data);
  } catch {
    return null;
  }
  if (message?.type !== 'removed') return null;
  return typeof message.reason === 'string' && message.reason !== '' ? message.reason : 'unknown';
}

/** The sentence for an API error code; unknown codes get a general one. */
export function messageFor(code) {
  return Object.hasOwn(MESSAGES, code) ? MESSAGES[code] : 'Something went wrong. Try again.';
}

/** The sentence for a refused "Inherit again": only an owner of the place above may do it. */
export function inheritAgainMessage(code) {
  return code === 'forbidden'
    ? 'Only an owner of the place above can make this inherit again.'
    : messageFor(code);
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
  element.append(...children.filter((child) => child != null && child !== false));
  return element;
}

/** Puts `text` in a message paragraph, which stays in the page (empty) so readers announce changes. */
export function showMessage(element, text) {
  element.textContent = text;
}


/** The roles that may change things in a place. */
export const canEdit = (role) => role === 'editor' || role === 'owner';

/**
 * The page that opens a node, by its kind. `editor` is a file's editor id; when it is not known
 * (a breadcrumb does not carry it) the file page opens and sends text documents on.
 */
export function pageFor(kind, id, editor) {
  switch (kind) {
    case 'site':
      return `/site.html?id=${seg(id)}`;
    case 'library':
    case 'folder':
      return `/library.html?id=${seg(id)}`;
    default:
      return editor === 'text' ? `/doc.html#${seg(id)}` : `/file.html?doc=${seg(id)}`;
  }
}

/**
 * The steps of a breadcrumb for `path` (site first, as `GET /api/nodes/{id}` answers it):
 * `{ id, name, href }`. `href` is null where the person has no role of their own (limited
 * access: the name is shown, not linked) and, unless `linkLast`, on the last step: the page
 * being shown.
 */
export function breadcrumbParts(path, { linkLast = true } = {}) {
  return path.map((crumb, at) => ({
    id: crumb.id,
    name: crumb.name,
    href:
      crumb.role && (linkLast || at < path.length - 1)
        ? pageFor(crumb.kind, crumb.id, crumb.editor)
        : null,
  }));
}

/** An ordered list of the steps of `path`: links where `breadcrumbParts` has an address. */
export function breadcrumbs(path, options) {
  const parts = breadcrumbParts(path, options);
  return h(
    'ol',
    {},
    ...parts.map((part, at) =>
      h(
        'li',
        at === parts.length - 1 ? { 'aria-current': 'page' } : {},
        part.href ? h('a', { href: part.href }, part.name) : h('span', {}, part.name),
      ),
    ),
  );
}

/**
 * Fills `target` (a `nav` element) with the breadcrumb of node `id` and returns what the page
 * needs: `{ path, role, folder }`, `folder` being the step above (where closing goes), or null
 * when there is none the person can open. Returns null, leaving `target` as it was, on failure.
 */
export async function showCrumbs(target, id, options) {
  const result = await api('GET', `/nodes/${seg(id)}`);
  if (!result.ok) return null;
  const path = result.data.path;
  target.replaceChildren(breadcrumbs(path, options));
  const above = path.length > 1 ? path[path.length - 2] : null;
  return { path, role: result.data.role, folder: above?.role ? above : null };
}

/** Roles a person can be given, lowest first. */
export const ROLES = Object.freeze(['viewer', 'editor', 'owner']);

/** `viewer` as `Viewer`. */
export const roleName = (role) => (role ? role[0].toUpperCase() + role.slice(1) : '');

/**
 * What the move dialog offers inside one place: sites, libraries and folders except `movingId`
 * itself, which is never offered, so nothing below it can be reached. Each has `canChoose`,
 * true only for a library or folder where the person is an editor or owner.
 */
export function moveChoices(items, movingId) {
  return items
    .filter((item) => item.id !== movingId && ['site', 'library', 'folder'].includes(item.kind))
    .map((item) => ({
      id: item.id,
      kind: item.kind,
      name: item.name,
      role: item.role ?? null,
      canChoose: item.kind !== 'site' && canEdit(item.role),
    }));
}

/**
 * Whether "Move here" is offered at the end of `trail` (the steps from the site to where the
 * dialog is): a library or folder the person can edit, which is not the item itself or below
 * it, and not the place the item is in already (`currentParentId`).
 */
export function canMoveHere(trail, movingId, currentParentId) {
  const here = trail[trail.length - 1];
  if (!here || !['library', 'folder'].includes(here.kind) || !canEdit(here.role)) return false;
  if (trail.some((step) => step.id === movingId)) return false;
  return here.id !== currentParentId;
}

/** A time from the server as the person's local date and time. */
export function formatTime(text) {
  const date = new Date(text);
  return Number.isNaN(date.getTime()) ? text : date.toLocaleString();
}

/** The id in the address's query (`?id=`), lowercased, or '' unless it is an id. */
export function queryId(search, name = 'id') {
  const id = (new URLSearchParams(search).get(name) ?? '').toLowerCase();
  return /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(id) ? id : '';
}

/**
 * Fills the account bar of a page: who is signed in, the People link for administrators and
 * Sign out. The page holds `who`, `people-link` and `sign-out`; `error` receives failures.
 */
export function setupTopbar(me, error) {
  const $ = (id) => document.getElementById(id);
  $('who').textContent = `Signed in as ${me.name}`;
  $('people-link').hidden = !me.is_admin;
  $('sign-out').addEventListener('click', async () => {
    const result = await api('DELETE', '/session');
    if (result.ok) location.assign('/signin.html');
    else showMessage(error, messageFor(result.error));
  });
}

/**
 * The site navigation: the libraries of site `siteId` as links in `target` (a `nav` element),
 * the one with id `currentId` marked as the current page.
 */
export async function showSiteNav(target, siteId, currentId) {
  const result = await api('GET', `/nodes/${seg(siteId)}/children?limit=500`);
  if (!result.ok) return;
  const libraries = result.data.items.filter((item) => item.kind === 'library');
  target.replaceChildren(
    h(
      'ul',
      {},
      ...libraries.map((library) =>
        h(
          'li',
          {},
          h(
            'a',
            { href: pageFor('library', library.id), 'aria-current': library.id === currentId ? 'page' : null },
            library.name,
          ),
        ),
      ),
    ),
  );
  target.hidden = libraries.length === 0;
}

/** Puts `text` in `element`, followed by a link to `folder` (a crumb) when there is one. */
export function showProblemWithFolder(element, text, folder) {
  element.textContent = text;
  if (folder) {
    element.append(' ', h('a', { href: pageFor(folder.kind, folder.id) }, `Go to ${folder.name}`));
  }
}
