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
  beyond_your_access: 'You can only give access you have yourself.',
  role_in_use: 'That role is still given somewhere. Change those entries first, then delete it.',
  builtin_role: 'Viewer, Editor and Owner are built in and cannot be changed or deleted.',
  group_in_use: 'That group still has access somewhere. Remove those entries first, then delete it.',
  site_group: 'A site always keeps its Owners, Members and Visitors groups.',
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


/**
 * Whether `access` (`{ perms, roles }` as the server answers it, or null for limited access)
 * holds the permission `perm`: `view`, `add`, `edit`, `delete`, `manage_access` or `manage_site`.
 */
export const can = (access, perm) => Array.isArray(access?.perms) && access.perms.includes(perm);

/** Whether `access` lets the person change content and rename. */
export const canEdit = (access) => can(access, 'edit');

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
 * `{ id, name, href }`. `href` is null where the person has no access of their own (limited
 * access: the name is shown, not linked) and, unless `linkLast`, on the last step: the page
 * being shown.
 */
export function breadcrumbParts(path, { linkLast = true } = {}) {
  return path.map((crumb, at) => ({
    id: crumb.id,
    name: crumb.name,
    href:
      crumb.access && (linkLast || at < path.length - 1)
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
 * needs: `{ path, access, folder }`, `folder` being the step above (where closing goes), or null
 * when there is none the person can open. Returns null, leaving `target` as it was, on failure.
 */
export async function showCrumbs(target, id, options) {
  const result = await api('GET', `/nodes/${seg(id)}`);
  if (!result.ok) return null;
  const path = result.data.path;
  target.replaceChildren(breadcrumbs(path, options));
  const above = path.length > 1 ? path[path.length - 2] : null;
  return { path, access: result.data.access, folder: above?.access ? above : null };
}

/** The permissions, in the order they are listed. `view` is part of every other one. */
export const PERMS = Object.freeze(['view', 'add', 'edit', 'delete', 'manage_access', 'manage_site']);

const PERM_LABELS = Object.freeze({
  view: 'view',
  add: 'add',
  edit: 'edit',
  delete: 'delete',
  manage_access: 'manage access',
  manage_site: 'manage site',
});

/** `manage_access` as `manage access`. */
export const permLabel = (perm) => PERM_LABELS[perm] ?? perm;

/** The permissions in `perms` as words in the order of [`PERMS`], or `none`. */
export function permsText(perms) {
  const held = PERMS.filter((perm) => Array.isArray(perms) && perms.includes(perm));
  return held.length ? held.map(permLabel).join(', ') : 'none';
}

/** The names of the roles in `access`, joined; '' for limited access. */
export const rolesText = (access) => (access?.roles ?? []).map((role) => role.name).join(', ');

/** "Your role: Owner" for `access`, or the line for limited access. */
export const roleLine = (access) =>
  access ? `Your role: ${rolesText(access)}` : 'Limited access: you can open only what was shared with you.';

/** What is shown for a group the person may not see. */
export const HIDDEN_GROUP = 'A group you cannot see';

/**
 * A person or group as the server names it in an assignment, as `{ name, note, hidden }`: the
 * note is a person's email, a site group's site (`site: Sales`: a site group can share its name
 * with an organisation group) or `organisation group`. A hidden group has no name to show.
 */
export function principalParts(principal) {
  if (principal.hidden) return { name: HIDDEN_GROUP, note: '', hidden: true };
  if (principal.kind === 'user') {
    return { name: principal.name, note: principal.email ?? '', hidden: false };
  }
  const note = principal.site ? `site: ${principal.site.name}` : 'organisation group';
  return { name: principal.name, note, hidden: false };
}

/** [`principalParts`] as one line: `Site Owners (site: Sales)`. */
export function principalText(principal) {
  const { name, note } = principalParts(principal);
  return note ? `${name} (${note})` : name;
}

/** A group from `GET /api/groups` (`{ name, site }`) as one line, as for an assignment. */
export const groupLabel = (group) => principalText({ kind: 'group', ...group });

/**
 * What the library page offers for one item, from the person's access to it: Rename (`edit`;
 * for a library `manage_site`), Move and Delete (`edit` and `delete`; never for a library) and
 * the Access link (`manage_access`).
 */
export function itemActions({ kind, access }) {
  const library = kind === 'library';
  return {
    rename: library ? can(access, 'manage_site') : can(access, 'edit'),
    move: !library && can(access, 'edit'),
    remove: !library && can(access, 'delete'),
    access: can(access, 'manage_access'),
  };
}

/**
 * The rows the explorer shows for the children `items` of one place (as `GET
 * /api/nodes/{id}/children` lists them): only those the person manages access on, each with its
 * break count from `counts` (a Map of id to count). `unique` says the item has permissions of
 * its own. With `onlyExceptions`, only breaks and the places with a break below them.
 */
export function explorerRows(items, counts, { onlyExceptions = false } = {}) {
  return items
    .filter((item) => can(item.access, 'manage_access'))
    .map((item) => ({
      id: item.id,
      kind: item.kind,
      name: item.name,
      access: item.access,
      unique: item.inherits === false,
      count: counts.get(item.id) ?? 0,
      expandable: item.kind !== 'document',
    }))
    .filter((row) => !onlyExceptions || row.unique || row.count > 0);
}

/** `open` (a Set of ids) with `id` shown or hidden: a new Set. */
export function toggleOpen(open, id) {
  const next = new Set(open);
  if (!next.delete(id)) next.add(id);
  return next;
}

/**
 * The rows on screen, in order, as `{ row, depth, open }`: `root` first, then the children of
 * every open row in `loaded` (a Map of id to rows), their own open rows below them in turn.
 */
export function visibleTree(root, loaded, open, depth = 0) {
  const isOpen = open.has(root.id);
  const out = [{ row: root, depth, open: isOpen }];
  if (isOpen) {
    for (const child of loaded.get(root.id) ?? []) out.push(...visibleTree(child, loaded, open, depth + 1));
  }
  return out;
}

/**
 * The query string of an explorer request: `principal` (`user:<id>` or `group:<id>`), `kinds`
 * (node kinds), `limit` and `after`; '' when none is given.
 */
export function explorerQuery({ principal, kinds, limit, after } = {}) {
  const query = new URLSearchParams();
  if (principal) query.set('principal', principal);
  if (kinds?.length) query.set('kind', kinds.join(','));
  if (limit) query.set('limit', String(limit));
  if (after) query.set('after', after);
  const text = query.toString();
  return text ? `?${text}` : '';
}

const plural = (n, one, many) => `${n} ${n === 1 ? one : many}`;

/**
 * The words of a restore preview (`GET /api/nodes/{id}/inheritance/preview`): where access would
 * come from, who loses or gains access (only from the entries the person can see, which the
 * heading says), the groups whose members cannot be named, and the breaks below that stay.
 */
export function previewSections(preview) {
  const change = (name, item) => `${name}: ${permsText(item.before)} → ${permsText(item.after)}`;
  const below = preview.unchanged_breaks_below;
  return {
    source: preview.inherit_from.name,
    peopleHeading: 'People who lose or gain access (changes from entries you can see)',
    peopleEmpty: 'No one you can see would lose or gain access.',
    people: preview.people.map((item) => change(item.user.name, item)),
    groupsHeading: 'Groups whose members are not listed',
    groups: preview.groups.map((item) => {
      const named = item.group.hidden ? 'a group you cannot see' : principalText(item.group);
      return change(`Members of ${named}`, item);
    }),
    below:
      below === 0
        ? 'No places below this one have access of their own.'
        : `${plural(below, 'place below keeps', 'places below keep')} their own access and ${
            below === 1 ? 'is' : 'are'
          } not changed.`,
  };
}

/** The preview as elements: a heading and a banded list for people, then groups, then a note. */
export function previewView(preview) {
  const s = previewSections(preview);
  const list = (items) => h('ul', { class: 'list' }, ...items.map((text) => h('li', {}, text)));
  return h(
    'div',
    {},
    h('p', {}, 'Access would come from ', h('strong', {}, s.source), ' instead.'),
    h('h3', {}, s.peopleHeading),
    s.people.length ? list(s.people) : h('p', { class: 'meta' }, s.peopleEmpty),
    s.groups.length ? h('h3', {}, s.groupsHeading) : null,
    s.groups.length ? list(s.groups) : null,
    h('p', { class: 'meta' }, s.below),
  );
}

/**
 * What the move dialog offers inside one place: sites, libraries and folders except `movingId`
 * itself, which is never offered, so nothing below it can be reached. Each has `canChoose`,
 * true only for a library or folder where the person may add (`add`).
 */
export function moveChoices(items, movingId) {
  return items
    .filter((item) => item.id !== movingId && ['site', 'library', 'folder'].includes(item.kind))
    .map((item) => ({
      id: item.id,
      kind: item.kind,
      name: item.name,
      access: item.access ?? null,
      canChoose: item.kind !== 'site' && can(item.access, 'add'),
    }));
}

/**
 * Whether "Move here" is offered at the end of `trail` (the steps from the site to where the
 * dialog is): a library or folder the person may add to, which is not the item itself or below
 * it, and not the place the item is in already (`currentParentId`).
 */
export function canMoveHere(trail, movingId, currentParentId) {
  const here = trail[trail.length - 1];
  if (!here || !['library', 'folder'].includes(here.kind) || !can(here.access, 'add')) return false;
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

/** The roles in `roles` whose permissions are all held in `access`: the ones this person may give. */
export const rolesWithin = (roles, access) =>
  roles.filter((role) => role.perms.every((perm) => can(access, perm)));

/** What the explorer says a break would inherit from when the server names no place. */
export const noSourceText = (kind) =>
  kind === 'site'
    ? 'Nothing: a site always has its own access.'
    : 'The place above, which you do not manage access on.';

/** How many breaks lie below an explorer row: its count, without the row itself if it is one. */
export const breaksBelow = (row) => Math.max(0, row.count - (row.unique ? 1 : 0));

/**
 * The explorer's "only exceptions" tree, built from the breaks themselves (never from the files
 * between them): `{ root, loaded, breaks }`. `root` is the row of the explorer's root, `loaded`
 * maps each row's id to its child rows (every row is open-able at once), `breaks` maps the id of
 * each break to what the server said about it. Rows are `{ id, kind, name, access, unique,
 * count, expandable }` as [`explorerRows`] makes them: `unique` is true for a break; `count` is
 * the breaks at or below it among those given. `root` is null when there are none.
 */
export function exceptionTree(breaks) {
  const rows = new Map();
  const loaded = new Map();
  const found = new Map();
  let root = null;
  for (const item of breaks) {
    const chain = [...item.path, item.node];
    let parent = null;
    chain.forEach((crumb, at) => {
      let row = rows.get(crumb.id);
      if (!row) {
        row = {
          id: crumb.id,
          kind: crumb.kind,
          name: crumb.name,
          access: crumb.access,
          unique: false,
          count: 0,
          expandable: crumb.kind !== 'document',
        };
        rows.set(crumb.id, row);
        if (parent) loaded.set(parent.id, [...(loaded.get(parent.id) ?? []), row]);
        else root ??= row;
      }
      row.count += 1;
      if (at === chain.length - 1) row.unique = true;
      parent = row;
    });
    found.set(item.node.id, item);
  }
  return { root, loaded, breaks: found };
}
