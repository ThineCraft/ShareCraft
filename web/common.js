// Shared by every page: calling the API, turning its error codes into sentences, and the
// sign-in redirect. Pages hold no merging or permission logic; the server decides.

import { CHECK_LABELS, MIN_RATIO, applyLook, parseColour } from './look.js';
import { setupAccountMenu, setupWordmark } from './account.js';

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
  forbidden_below:
    'Something inside that folder has its own permissions and you may not delete it, so the folder cannot be deleted.',
  archived:
    'This site is archived, so nothing in it can be changed. An administrator can reactivate it.',
  network: 'Could not reach the server. Check the connection and try again.',
  too_large: 'That file is too large.',
  unknown_editor: 'That kind of document cannot be made here.',
  upload_required: 'A PDF cannot be made empty. Upload a PDF file instead.',
  wrong_extension: 'That name does not end with the extension of this kind of document.',
  name_taken: 'Something here already has that name. Choose a different name.',
  quota_exceeded:
    'This site has no room left under its storage quota. Delete or purge what is not needed, or ask an administrator for more.',
  parent_in_bin:
    'A folder this item was in is in the recycle bin, and you cannot restore it. Ask whoever deleted that folder to restore it first.',
  into_itself: 'A folder cannot be moved into itself or into a folder inside it.',
  wrong_place: 'That cannot go there. Choose a library or a folder.',
  is_site: 'A site cannot be moved, deleted or made to inherit.',
  site_deleted:
    'That site is deleted. An administrator can restore it from the Sites page, or purge it.',
  not_deleted: 'That site is not deleted, so there is nothing to restore or purge.',
  not_a_hub: 'That site is not a hub. An administrator can make it one first.',
  is_hub:
    'A hub cannot join another hub, and a site that has joined a hub cannot become one. Leave the hub first.',
  invalid_request: 'That change is not allowed. A file keeps its extension when renamed.',
  unavailable: 'The server is busy or its database is unavailable. Try again in a moment.',
  bad_request: 'The server could not read that request. Reload the page and try again.',
  not_json: 'The server could not read that request. Reload the page and try again.',
  link_not_found: 'This link does not work. It may have expired or been turned off.',
  link_group: 'That group belongs to a sharing link. Change or revoke the link instead.',
  link_role_too_high: 'A link cannot give the right to manage access or the site. Choose a different role.',
  already_has: 'That person already has at least that much access here.',
  anyone_links_off: 'Links for anyone are switched off. An administrator can switch them on.',
  expiry_required: 'A link for anyone needs an expiry. Enter a number of days.',
  expiry_too_long: 'That expiry is further away than the organisation allows. Choose fewer days.',
  expiry_in_past: 'That expiry has already passed. Choose at least one day.',
  expiry_invalid: 'Enter a whole number of days, 1 or more.',
  too_many_requests: 'You have asked for access too many times today. Try again tomorrow.',
  already_decided: 'Someone has already decided that request. Reload the page.',
  message_too_long: 'That message is too long. Use 500 characters or fewer.',
  busy: 'The server is busy right now. Try again in a moment.',
  being_edited: 'Someone is editing that file right now. Try again when they have finished.',
  text_document: 'That is a text document. Open it and edit it in its own page.',
  checksum_mismatch:
    'The file did not arrive intact (it may have changed while it was being sent). Nothing was saved; try again.',
  too_many_uploads: 'You have too many uploads open at once. Wait for some to finish, then try again.',
  invalid_range: 'The server could not take that part of the file. Try the upload again.',
  wrong_offset: 'The upload got out of step with the server. Try the upload again.',
  invalid_language: 'That is not a language code. Use one like en, en-GB or pt-BR.',
  invalid_time_zone: 'That is not a known time zone. Use one like UTC or Europe/London.',
  invalid_navigation:
    'That navigation is not allowed. Check the labels (60 characters at most), the addresses (https:, http:, mailto: or a path) and that links go one level deep.',
  folders_off:
    'This library does not take folders. Its owners can switch folders on in the library settings.',
  blocked_file_type:
    'Files of that type are not allowed here. The library or the organisation blocks it; choose another file or another library.',
  file_too_large:
    'That file is larger than this library takes. Choose a smaller file or another library.',
  held:
    'This file cannot be opened yet: it is waiting for the malware scanner, or the scanner found a threat in it. An administrator can release it.',
  no_preview:
    'This file has no preview. Download it to open it.',
  no_thumbnail:
    'There is no thumbnail for this file.',
  invalid_scanner_url:
    'That is not a scanner address. Write it as icap://host/service or icap://host:port/service, with no user name or password.',
  invalid_library_settings:
    'Those library settings are not allowed. File types are written like .png (at most 100 of them), and an image library accepts only image types.',
  invalid_template:
    'That template cannot be used. It may be too large (at most 100 libraries, 5,000 folders and 20 levels), name things by identifier, or have a link that is not allowed.',
  low_contrast:
    'Those colours are too hard to read: some text or buttons fall below the contrast people need, in light or dark mode. Choose a suggested shade instead.',
  invalid_look:
    'That look cannot be applied. Check the header, footer and logo choices and the footer links (https:, http: or a path).',
  invalid_theme:
    'That theme cannot be used. Give it a name of 1 to 60 characters and colours written like #3156c9.',
  not_an_image: 'That file is not an image ShareCraft can show. Use a PNG, JPEG, WebP, GIF or SVG image.',
  image_too_large:
    'That image is too large. A logo, thumbnail or icon can be at most 1 MB, a background at most 5 MB, and no more than 8,000 pixels across.',
  image_gone:
    'An image of that look no longer exists. Choose new images for the parts that lost theirs, then apply it again.',
  too_many: 'There are already as many of those as allowed. Delete one first.',
};

/** What a document page says when the document is missing or the person has no access. */
export const MISSING_DOCUMENT = 'This document does not exist, or you do not have access to it.';

const REMOVED = Object.assign(Object.create(null), {
  access_revoked: 'Your access to this document was removed.',
  signed_out: 'You were signed out.',
  account_disabled: 'Your account was disabled.',
  deleted: 'This document was deleted.',
  archived: 'The site of this document was archived. It can still be read, but not changed.',
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
 * `body` is sent as JSON; with `{ raw: true }` it is sent as it is (the text of a template
 * package, say).
 */
export async function api(method, path, body, { raw = false } = {}) {
  const init = { method, credentials: 'same-origin', headers: {} };
  if (body !== undefined) {
    init.headers['Content-Type'] = 'application/json';
    init.body = raw ? body : JSON.stringify(body);
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
export function principalParts(principal, role) {
  if (principal.kind === 'link') return { name: linkLabel(principal, role), note: '', hidden: false };
  if (principal.hidden) return { name: HIDDEN_GROUP, note: '', hidden: true };
  if (principal.kind === 'user') {
    return { name: principal.name, note: principal.email ?? '', hidden: false };
  }
  const note = principal.site ? `site: ${principal.site.name}` : 'organisation group';
  return { name: principal.name, note, hidden: false };
}

/** [`principalParts`] as one line: `Site Owners (site: Sales)`. */
export function principalText(principal, role) {
  const { name, note } = principalParts(principal, role);
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
 * The entries of an item's Actions menu on the library page, in order: `rename`, `move`, `copy`,
 * `delete` and `access`, each only where the person may. Copy needs only sight of the item (the
 * place it goes to is checked in the dialog), but a library is never copied.
 */
export function rowMenu(item) {
  const allowed = itemActions(item);
  const entries = [];
  if (allowed.rename) entries.push('rename');
  if (allowed.move) entries.push('move');
  if (item.kind !== 'library') entries.push('copy');
  if (allowed.remove) entries.push('delete');
  if (allowed.access) entries.push('access');
  return entries;
}

/** What each new-document kind is called in the New menu. */
export const NEW_DOC_LABELS = Object.freeze({
  text: 'Text document',
  gridcraft: 'Spreadsheet (.xlsx)',
  wordcraft: 'Document (.docx)',
  solidcraft: 'Design (.solidcraft.json)',
});

/**
 * The entries of the library page's New menu, in order, as `{ key, label }`: Folder (unless the
 * library takes no folders), each document kind in `editors`, From a template (when there are
 * templates), then Upload files and Upload a folder.
 */
export function newMenu({ folders = true, editors = [], templates = 0 } = {}) {
  const entries = [];
  if (folders !== false) entries.push({ key: 'folder', label: 'Folder' });
  for (const editor of editors) {
    if (Object.hasOwn(NEW_DOC_LABELS, editor)) entries.push({ key: `doc:${editor}`, label: NEW_DOC_LABELS[editor] });
  }
  if (templates > 0) entries.push({ key: 'template', label: 'From a template…' });
  entries.push({ key: 'files', label: 'Upload files' }, { key: 'folder-upload', label: 'Upload a folder' });
  return entries;
}

/**
 * What the explorer's "who" filter offers: the people and groups named on `assignments` (the
 * explorer's root) and on every break in `breaks` (as `GET /api/explorer/{root}` lists them),
 * the `groups` listed (`GET /api/groups`) and `members` (people seen as members of groups,
 * `{ id, name }`). Groups hidden from the person are left out. `{ people, groups }`, each a list
 * of `[id, label]` with every id once, sorted by label.
 */
export function filterChoices({ assignments = [], breaks = [], groups = [], members = [] }) {
  const people = new Map();
  const named = new Map();
  const take = (principal) => {
    if (principal.hidden || principal.kind === 'link') return;
    if (principal.kind === 'user') people.set(principal.id, principal.name);
    else named.set(principal.id, groupLabel(principal));
  };
  for (const { principal } of assignments) take(principal);
  for (const found of breaks) for (const { principal } of found.assignments ?? []) take(principal);
  for (const group of groups) named.set(group.id, groupLabel(group));
  for (const member of members) people.set(member.id, member.name);
  const sorted = (map) =>
    [...map].sort(([idA, a], [idB, b]) => a.localeCompare(b) || (idA < idB ? -1 : idA > idB ? 1 : 0));
  return { people: sorted(people), groups: sorted(named) };
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
  const moving = [].concat(movingId);
  return items
    .filter((item) => !moving.includes(item.id) &&['site', 'library', 'folder'].includes(item.kind))
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

/**
 * Whether "Move here" or "Copy here" is offered at the end of `trail` for the items `ids`: a
 * library or folder the person may add to, with none of the items in the trail (never into
 * itself or below). A move is not offered where the items are already (`currentParentId`); a
 * copy is (it is kept alongside under a new name).
 */
export function canPlaceHere(trail, ids, currentParentId, { copy = false } = {}) {
  const here = trail[trail.length - 1];
  if (!here || !['library', 'folder'].includes(here.kind) || !can(here.access, 'add')) return false;
  if (trail.some((step) => ids.includes(step.id))) return false;
  return copy || here.id !== currentParentId;
}

/** A time from the server in the page's language and time zone (see [`formatDate`]). */
export const formatTime = (text) => formatDate(text);

/**
 * What an anyone link's page offers for a file with `revised_at` (as the server answers it,
 * absent or null when never saved): `{ download, saved }`. A file never saved has nothing to
 * download, so `download` is false and `saved` is `Not saved yet`, shown in its place; otherwise
 * `saved` is `Last saved <local time>`.
 */
export function fileOffer(revisedAt) {
  if (revisedAt == null) return { download: false, saved: 'Not saved yet' };
  return { download: true, saved: `Last saved ${formatTime(revisedAt)}` };
}

/** The id in the address's query (`?id=`), lowercased, or '' unless it is an id. */
export function queryId(search, name = 'id') {
  const id = (new URLSearchParams(search).get(name) ?? '').toLowerCase();
  return /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(id) ? id : '';
}

/**
 * Fills the account bar (`.topbar`) of a page: the ShareCraft wordmark linking to the start page,
 * the command palette's search button and the avatar with the account menu (account.js): who is
 * signed in, People and Brand centre for those who may use them, Appearance and Sign out. `error`
 * receives failures.
 */
export function setupTopbar(me, error) {
  const bar = document.querySelector('.topbar');
  if (bar) {
    setupWordmark(bar);
    if (!bar.querySelector('.spacer')) bar.append(h('span', { class: 'spacer' }));
    const brandManager = me.is_admin
      ? Promise.resolve(true)
      : api('GET', '/brand').then((result) => Boolean(result.ok && result.data?.can_manage));
    setupAccountMenu(bar, me, {
      brandManager,
      signOut: async () => {
        const result = await api('DELETE', '/session');
        if (result.ok) location.assign('/signin.html');
        else if (error) showMessage(error, messageFor(result.error));
      },
    });
  }
  startPalette(me);
}

/**
 * Starts the command palette (palette.js) for the signed-in person `me`: Ctrl+K, Cmd+K or "/"
 * opens it, and the account bar gets its button. Loaded on demand, so pages that never need it
 * (and the tests) do not load it; setupTopbar calls this, pages without it call it themselves.
 */
export function startPalette(me) {
  if (typeof document === 'undefined') return;
  import('./palette.js')
    .then((palette) => palette.setupPalette(me))
    .catch(() => {
      // without the palette the page still works through its own links
    });
}

/** Puts `text` in `element`, followed by a link to `folder` (a crumb) when there is one. */
export function showProblemWithFolder(element, text, folder) {
  element.textContent = text;
  if (folder) {
    element.append(' ', h('a', { href: pageFor(folder.kind, folder.id) }, `Go to ${folder.name}`));
  }
}

/**
 * The nearest place above the last step of `path` (a breadcrumb path) that still exists and
 * that the person may open, or null. `exists(id)` answers true, false, or null when it could
 * not be told (then the search stops and nothing is linked).
 */
export async function nearestLivingAncestor(path, exists) {
  for (let at = path.length - 2; at >= 0; at -= 1) {
    const step = path[at];
    if (!step?.access) continue;
    const there = await exists(step.id);
    if (there === null) return null;
    if (there) return step;
  }
  return null;
}

/** Whether node `id` can still be read: true, false for "gone or hidden", null for "could not tell". */
export async function nodeStillThere(id) {
  const result = await api('GET', `/nodes/${seg(id)}`);
  if (result.ok) return true;
  return result.error === 'network' ? null : false;
}

/** Adds a link to the nearest place above `path` that still exists, when one is found. */
export async function linkNearestPlace(element, path) {
  const place = await nearestLivingAncestor(path, nodeStillThere);
  if (place) element.append(' ', h('a', { href: pageFor(place.kind, place.id) }, `Go to ${place.name}`));
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

// ---- sharing links and access requests ----

/** What the link page says for any link that does not work, whatever the reason. */
export const LINK_NOT_WORKING = 'This link does not work. It may have expired or been turned off.';

/** What is said, once, beside the address of a link that was just made. */
export const ANYONE_NOTICE = 'This address is shown only now. If you lose it, make a new link.';

/** What every request for access is answered with, whether or not the item exists. */
export const REQUEST_SENT = 'If this item exists, the people who manage it will see your request.';

/**
 * What a request for access shows after the server's answer `error` (null when it was accepted).
 * Only the two limits have sentences of their own; every other outcome, a network failure
 * included, reads as the accepted answer, so the page can never reveal whether the item exists.
 */
export function requestAnswer(error) {
  return error === 'too_many_requests' || error === 'message_too_long' ? messageFor(error) : REQUEST_SENT;
}

/** The longest message with a request for access, or reply to one, in characters. */
export const REQUEST_MAX = 500;

/** `'message_too_long'` when `text` is over [`REQUEST_MAX`] characters, otherwise null. */
export function requestProblem(text) {
  return [...String(text ?? '')].length > REQUEST_MAX ? 'message_too_long' : null;
}

/** The roles a link may give: within `access` (as for any sharing) and never managing access or the site. */
export const linkRoleChoices = (roles, access) =>
  rolesWithin(roles, access).filter(
    (role) => !role.perms.includes('manage_access') && !role.perms.includes('manage_site'),
  );

/**
 * The expiry of a link for people: `expires 1 December`, with the year when it is not the year of
 * `now`; `never expires` for none. `now` (milliseconds), `timeZone` and `locale` can be passed in;
 * they default to the page's (see [`setRegional`]).
 */
export function expiryText(iso, { now = Date.now(), timeZone = regional.timeZone, locale = regional.language ?? 'en-GB' } = {}) {
  if (iso == null) return 'never expires';
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return String(iso);
  const year = (value) => new Date(value).toLocaleDateString('en-GB', { year: 'numeric', timeZone });
  const options = { day: 'numeric', month: 'long', timeZone };
  if (year(date) !== year(now)) options.year = 'numeric';
  return `expires ${date.toLocaleDateString(locale, options)}`;
}

/**
 * A link as one phrase: `Signed-in link (Editor, expires 1 December)`. `link` is a link from
 * `GET /api/nodes/{id}/links` (`kind`) or a link entry of an assignment (`link_kind`); `role` is
 * `{ name }` or null.
 */
export function linkLabel(link, role, options) {
  const kind = link.link_kind ?? link.kind;
  const what = kind === 'anyone' ? 'Anyone link' : 'Signed-in link';
  return `${what} (${role?.name ? `${role.name}, ` : ''}${expiryText(link.expires_at, options)})`;
}

/**
 * Where a link's assignment shown on the access page of `hereId` is revoked: `{ text, on }`. On
 * the link's own item, under Share by link on this page (`on` null); on a break below that holds a
 * copy, on the access page of the link's item (`principal.item`, `{ id, name }`): `on` is
 * `{ name, href }`, shown as `... on <a href>name</a>`. When the item's `name` is null (the person
 * may not see that item), plain text naming no item, with `on` null.
 */
export function linkRevokeHint(principal, hereId) {
  const text = 'Revoke it under Share by link';
  const item = principal.item;
  if (!item || item.id === hereId) return { text, on: null };
  if (item.name == null) return { text: `${text} on the item it was made on.`, on: null };
  return { text, on: { name: item.name, href: `/access.html?id=${seg(item.id)}` } };
}

/** `Anyone links: 2 live` for a break with anyone links; '' for none. */
export const anyoneLinksText = (count) => (count > 0 ? `Anyone links: ${count} live` : '');

/** The links of `links` that are neither revoked nor past their expiry at `now`. */
export const liveLinks = (links, now = Date.now()) =>
  links.filter((link) => !link.revoked && (link.expires_at == null || Date.parse(link.expires_at) > now));

/**
 * An expiry typed as a number of days from `now`: `{ at, error }` with `at` an RFC 3339 time (null
 * for no expiry) and `error` null or an error code (`expiry_required`, `expiry_invalid`,
 * `expiry_too_long`) for `messageFor`.
 */
export function expiryFromDays(text, { maxDays, required, now = Date.now() }) {
  const typed = String(text ?? '').trim();
  if (typed === '') return { at: null, error: required ? 'expiry_required' : null };
  if (!/^[0-9]{1,5}$/.test(typed) || Number(typed) < 1) return { at: null, error: 'expiry_invalid' };
  const days = Number(typed);
  if (days > maxDays) return { at: null, error: 'expiry_too_long' };
  return { at: new Date(now + days * 86400000).toISOString(), error: null };
}

/** `Approved as Editor` or `Declined` for one of the person's own decided requests. */
export function requestDecisionText(request) {
  if (request.decision === 'approved') {
    return request.role?.name ? `Approved as ${request.role.name}` : 'Approved';
  }
  return 'Declined';
}

/** How long before `now` a time was: `just now`, `5 minutes ago`, `1 hour ago`, `3 days ago`. */
export function ageText(iso, now = Date.now()) {
  const seconds = Math.floor((now - Date.parse(iso)) / 1000);
  if (!(seconds >= 60)) return 'just now';
  const unit = (n, name) => `${n} ${name}${n === 1 ? '' : 's'} ago`;
  if (seconds < 3600) return unit(Math.floor(seconds / 60), 'minute');
  if (seconds < 86400) return unit(Math.floor(seconds / 3600), 'hour');
  return unit(Math.floor(seconds / 86400), 'day');
}

/**
 * `choices` (`{ people, groups }` as [`filterChoices`] answers) with the entry for `chosen`
 * (`user:<id>` or `group:<id>`, '' for none) added, named `label`, if it is not there: so a
 * filter never shows another entry than the one it filters by. A new object.
 */
export function withChosen(choices, chosen, label) {
  const out = { people: [...choices.people], groups: [...choices.groups] };
  const at = chosen.indexOf(':');
  if (at < 0) return out;
  const list = chosen.slice(0, at) === 'user' ? out.people : out.groups;
  const id = chosen.slice(at + 1);
  if (!list.some(([known]) => known === id)) list.push([id, label || id]);
  return out;
}

/**
 * Puts, after the element `after`, the form that asks for access to node `id`. For every "does
 * not exist, or you do not have access" message: the answer is the same whether or not the item
 * exists. A new call replaces an earlier form; no `id` only removes it.
 */
export function offerRequest(after, id) {
  after.parentNode?.querySelector('[data-request-form]')?.remove();
  if (!id) return;
  const area = h('textarea', { id: 'request-message', rows: 3, maxlength: REQUEST_MAX, autocomplete: 'off' });
  const problem = h('p', { role: 'alert', class: 'msg' });
  const send = h('button', { type: 'submit' }, 'Request access');
  const form = h(
    'form',
    { 'data-request-form': '', class: 'request-form' },
    h('label', { for: 'request-message' }, `Ask the people who manage it for access (a message is optional, up to ${REQUEST_MAX} characters)`),
    area,
    h('div', {}, send),
    problem,
  );
  form.addEventListener('submit', async (event) => {
    event.preventDefault();
    showMessage(problem, '');
    const bad = requestProblem(area.value);
    if (bad) return showMessage(problem, messageFor(bad));
    send.disabled = true;
    const message = area.value.trim();
    const answer = await api('POST', `/nodes/${seg(id)}/requests`, message ? { message } : {});
    send.disabled = false;
    const shown = requestAnswer(answer.ok ? null : answer.error);
    if (shown !== REQUEST_SENT) return showMessage(problem, shown);
    form.replaceChildren(h('p', { role: 'status' }, REQUEST_SENT));
  });
  after.after(form);
}

// ---- files: recycle bin, bulk operations, quota, confirmations ----

/** How long an item in the recycle bin has left, from the server's `days_left`. */
export function daysLeftText(days) {
  if (days == null) return '';
  if (days <= 0) return 'Erased within a day';
  return days === 1 ? '1 day left' : `${days} days left`;
}

/**
 * What a recycle bin entry is and where it was, from its `kind` and `path` (the names of the
 * folders above it in its library): "Folder in Plans / 2026", "File at the top of its library",
 * or for a whole library "Library, with everything in it".
 */
export function binEntryText(entry) {
  if (entry.kind === 'library') return 'Library, with everything in it';
  const what = entry.kind === 'folder' ? 'Folder' : 'File';
  const where = entry.path?.length ? `in ${entry.path.join(' / ')}` : 'at the top of its library';
  return `${what} ${where}`;
}

/** `n` bytes in the largest unit that keeps it at 1 or more, in the page's language (see [`formatSize`]). */
export const bytesText = (n) => formatSize(n);

const itemsText = (n) => `${n} ${n === 1 ? 'item' : 'items'}`;
const capital = (text) => text.charAt(0).toUpperCase() + text.slice(1);
const BULK_DONE = {
  move: 'moved',
  copy: 'copied',
  delete: 'moved to the recycle bin',
  restore: 'restored',
};

/**
 * What a bulk request did, from the server's results (`[{id, ok, error?, new_id?, renamed?}]`)
 * and the names of the items by id: `{ ok, failed, headline, lines }`, one line (`{ name, ok,
 * text }`) per item in the order given, a rename (`renamed {from, to}`) shown on its line.
 */
export function bulkSummary(op, results, names) {
  const done = BULK_DONE[op] ?? 'changed';
  const ok = results.filter((result) => result.ok).length;
  const failed = results.length - ok;
  const was = (n) => (n === 1 ? 'was' : 'were');
  let headline;
  if (failed === 0) headline = `${itemsText(ok)} ${was(ok)} ${done}.`;
  else if (ok === 0) headline = `No items were ${done}. ${failed} could not be.`;
  else headline = `${ok} of ${itemsText(results.length)} ${was(ok)} ${done}. ${failed} could not be.`;
  const lines = results.map((result) => {
    const name = names[result.id] ?? 'An item';
    let text;
    if (!result.ok) text = messageFor(result.error);
    else if (result.renamed) {
      text = `${capital(done)} as “${result.renamed.to}”, because “${result.renamed.from}” was taken.`;
    } else text = `${capital(done)}.`;
    return { name, ok: result.ok, text };
  });
  return { ok, failed, headline, lines };
}

/** Whether none, some or all of `listed` (item ids) are in the set `selected`. */
export function selectionState(selected, listed) {
  const count = listed.filter((id) => selected.has(id)).length;
  const all = listed.length > 0 && count === listed.length;
  return { count, all, some: count > 0 && !all };
}

/** The share of `quota` used, from 0 to 100; `null` when there is no quota. */
export function usagePercent(used, quota) {
  if (quota === null || quota === undefined) return null;
  if (quota <= 0) return 100;
  return Math.min(100, Math.round((used / quota) * 100));
}

/** The site page's storage line, for people who manage the site (`GET /api/sites/{id}`). */
export function quotaText({ used, quota, quota_warning: warning }) {
  if (quota === null || quota === undefined) {
    return `This site holds ${bytesText(used)}. It has no storage quota.`;
  }
  return (
    `This site holds ${bytesText(used)} of its ${bytesText(quota)} storage quota.` +
    (warning
      ? ` It ${used >= quota ? 'is full' : 'is nearly full'}: delete or purge what is not needed, or ask an administrator for more.`
      : '')
  );
}

/**
 * Why a file did not fit: the space situation of its site in plain words. `site` is the
 * site's storage (`{ used, quota }`) when the person may see it, otherwise null or without
 * the numbers; `file` is `{ name, size }`.
 */
export function quotaRefusalText(site, file) {
  const head = `There is no room for “${file.name}” (${bytesText(file.size)}).`;
  if (site && typeof site.used === 'number' && typeof site.quota === 'number') {
    return (
      `${head} The site holds ${bytesText(site.used)} of its ${bytesText(site.quota)} storage quota. ` +
      'Delete or purge what is not needed, or ask an administrator for more.'
    );
  }
  return `${head} The site has no room left under its storage quota. Ask someone who manages the site to free some space, or an administrator for more.`;
}

/** The confirmation before emptying a bin stage of `count` items. */
export function emptyBinText(stage, count) {
  if (stage === 'second') {
    return `Permanently erase ${itemsText(count)}? Their files are erased from the server and this cannot be undone.`;
  }
  return (
    `Move ${itemsText(count)} to the second stage of the recycle bin? ` +
    'You will not be able to restore them yourself afterwards: only people who manage the site can bring them back, until they are erased for good.'
  );
}

/** The confirmation before removing `member` from `group`. */
export const removeMemberText = (member, group) =>
  `Remove ${member.name} from ${group.name}? They lose any access that comes from this group.`;

/** The confirmation before deleting `role`. */
export const deleteRoleText = (role) => `Delete the role ${role.name}? This cannot be undone.`;

/**
 * The quota in bytes for the text of the quota field (gigabytes, decimals allowed): `null`
 * for an empty field (no quota), `undefined` when it is not a number.
 */
export function parseQuotaGb(text) {
  const trimmed = String(text).trim();
  if (trimmed === '') return null;
  if (!/^\d+(\.\d+)?$/.test(trimmed)) return undefined;
  return Math.round(Number(trimmed) * 1024 ** 3);
}

/**
 * Asks the person to confirm before something is done: a dialog with `text`, a Cancel button
 * and a `label` button. Resolves true for the label, false for Cancel or Escape. The text is
 * put in as text only. A button click answers at once: a hidden or background tab may never
 * deliver the dialog's `close` event, which only Escape relies on.
 */
export function confirmDialog(text, label) {
  return new Promise((resolve) => {
    const dialog = h('dialog', { 'aria-label': label });
    const cancel = h('button', { type: 'button', class: 'plain' }, 'Cancel');
    const confirm = h('button', { type: 'button' }, label);
    dialog.append(h('p', {}, text), h('div', { class: 'row end' }, cancel, confirm));
    let answered = false;
    const answer = (yes) => {
      if (answered) return;
      answered = true;
      if (dialog.open) dialog.close();
      dialog.remove();
      resolve(yes);
    };
    cancel.addEventListener('click', () => answer(false));
    confirm.addEventListener('click', () => answer(true));
    dialog.addEventListener('close', () => answer(false));
    document.body.append(dialog);
    dialog.showModal();
    cancel.focus();
  });
}

/**
 * What the quota form sends: `{ bytes }` (null for no quota) or `{ error }`. Where the person
 * cannot see the current quota (`blind`), an empty field is refused rather than taken to mean
 * "remove the quota"; removing it has its own confirmed button. The site page always passes
 * `blind`, so nobody removes a quota by leaving the field empty.
 */
export function quotaSubmit(text, { blind = false } = {}) {
  const bytes = parseQuotaGb(text);
  const asked = 'Enter the quota as a number of gigabytes' + (blind ? '.' : ', or leave it empty for no quota.');
  if (bytes === undefined || (blind && bytes === null)) return { error: asked };
  return { bytes };
}

// ---- sites: regional formatting, navigation bars and the site directory ----

/** `language` if `Intl` knows it as a language tag, otherwise undefined. */
function knownLanguage(language) {
  if (typeof language !== 'string' || language === '') return undefined;
  try {
    return Intl.getCanonicalLocales(language)[0];
  } catch {
    return undefined;
  }
}

/** `timeZone` if `Intl` knows the zone, otherwise undefined. */
function knownZone(timeZone) {
  if (typeof timeZone !== 'string' || timeZone === '') return undefined;
  try {
    new Intl.DateTimeFormat('en', { timeZone });
    return timeZone;
  } catch {
    return undefined;
  }
}

// The language and time zone the page shows dates and sizes in: the site's, or the
// organisation's outside a site. Until a page sets them, the browser's own are used for dates
// and English for sizes, as before.
let regional = { language: undefined, timeZone: undefined };

/**
 * Sets the language and time zone of the page (`{ language, timeZone }`, as a site or the
 * organisation defaults give them) and `<html lang>`. A value `Intl` does not know is ignored.
 */
export function setRegional({ language, timeZone } = {}) {
  regional = { language: knownLanguage(language), timeZone: knownZone(timeZone) };
  const root = globalThis.document?.documentElement;
  if (root && regional.language) root.lang = regional.language;
}

/** The page's language and time zone as set by [`setRegional`] (either may be undefined). */
export function regionalSettings() {
  return { ...regional };
}

/**
 * A time from the server (RFC 3339, UTC) as a date and time in `language` and `timeZone`, with
 * the zone's short name: `14 Nov 2026, 15:04 GMT`. Either left out is the page's own (see
 * [`setRegional`]). '' for none; anything that is not a time is shown as it came.
 */
export function formatDate(iso, { language, timeZone } = {}) {
  if (iso == null) return '';
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return String(iso);
  const options = {
    day: 'numeric',
    month: 'short',
    year: 'numeric',
    hour: 'numeric',
    minute: '2-digit',
    timeZoneName: 'short',
    timeZone: knownZone(timeZone) ?? regional.timeZone,
  };
  return new Intl.DateTimeFormat(knownLanguage(language) ?? regional.language, options).format(date);
}

/**
 * `bytes` in the largest unit that keeps it at 1 or more, its number written in `language` (the
 * page's when left out; English when there is none): `1.5 KB`, `1,5 KB`.
 */
export function formatSize(bytes, language) {
  if (typeof bytes !== 'number' || !Number.isFinite(bytes)) return String(bytes);
  const units = ['bytes', 'KB', 'MB', 'GB', 'TB'];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  const locale = knownLanguage(language) ?? regional.language ?? 'en';
  const digits = unit === 0 ? { maximumFractionDigits: 0, useGrouping: false } : { minimumFractionDigits: 1, maximumFractionDigits: 1 };
  return `${new Intl.NumberFormat(locale, digits).format(value)} ${units[unit]}`;
}

/** The longest address a navigation link may have, as the server allows. */
const MAX_URL = 2000;

/**
 * `url` if it may be put into `href`: `https:`, `http:`, `mailto:` or a path starting with a
 * single `/`, at most 2,000 characters, with no control characters and no space at either end.
 * Otherwise null, and the page shows the label as text. The server refuses anything else
 * already; this is checked again on the page.
 */
export function safeHref(url) {
  if (typeof url !== 'string' || url === '' || url.length > MAX_URL || url !== url.trim()) return null;
  for (const ch of url) {
    const code = ch.codePointAt(0);
    if (code < 0x20 || code === 0x7f) return null;
  }
  if (url.startsWith('/')) return url[1] === '/' || url[1] === '\\' ? null : url;
  const scheme = /^([a-zA-Z][a-zA-Z0-9+.-]*):/.exec(url)?.[1].toLowerCase();
  return scheme === 'https' || scheme === 'http' || scheme === 'mailto' ? url : null;
}

/**
 * The address a resolved navigation target opens, or null (a heading, or a refused address). A
 * document opens the page of its editor when the target names it (`editor: "text"` opens the
 * text page). The server's answer does not name it yet, so a document opens the file page,
 * which sends a text document on to its text page.
 */
export function navHref(target) {
  if (!target) return null;
  if (typeof target.node === 'string') return pageFor(target.kind, target.node, target.editor);
  return safeHref(target.url);
}

let menus = 0; // makes the id of each drawn menu unique on the page

/** Opens or closes the list `list` under the disclosure button `button`. */
function setOpen(button, list, open) {
  button.setAttribute('aria-expanded', open ? 'true' : 'false');
  list.hidden = !open;
}

/** One navigation item, with its children under a disclosure button. Text only, never markup. */
function navItem(item, currentId) {
  const href = navHref(item.target);
  const current = item.target?.node != null && item.target.node === currentId;
  const label = href
    ? h('a', { href, 'aria-current': current ? 'page' : null }, item.label)
    : item.children?.length
      ? null
      : h('span', { class: 'heading' }, item.label);
  const targeted = item.targeted
    ? h('small', { class: 'mark', title: 'Shown only to the groups it is aimed at, and to you because you manage it' }, 'Targeted')
    : null;
  if (!item.children?.length) return h('li', {}, label, targeted);
  menus += 1;
  const id = `nav-sub-${menus}`;
  const sub = h('ul', { class: 'sub', id }, ...item.children.map((child) => navItem(child, currentId)));
  sub.hidden = true;
  const toggle = label
    ? h('button', { type: 'button', class: 'sub-toggle', 'aria-expanded': 'false', 'aria-controls': id, 'aria-label': `More under ${item.label}` }, '▾')
    : h('button', { type: 'button', class: 'sub-toggle heading', 'aria-expanded': 'false', 'aria-controls': id }, item.label, ' ▾');
  toggle.addEventListener('click', () => setOpen(toggle, sub, toggle.getAttribute('aria-expanded') !== 'true'));
  return h('li', { class: 'has-sub' }, label, toggle, targeted, sub);
}

/** One bar: a name (a link to `head.href` when there is one) and its items. */
function navBar(kind, label, head, items, currentId) {
  const thumb = head?.thumb ? brandImage(head.thumb, { class: 'navthumb', alt: '' }) : null;
  const name =
    head?.name == null
      ? null
      : head.href
        ? h('a', { class: 'navhead', href: head.href }, thumb, head.name)
        : h('span', { class: 'navhead' }, thumb, head.name);
  return h(
    'div',
    { class: `navbar ${kind}`, 'data-bar': kind },
    name,
    items.length ? h('ul', { 'aria-label': label }, ...items.map((item) => navItem(item, currentId))) : null,
  );
}

/**
 * The navigation shown on a page: the organisation's global bar, the hub's bar (its name linking
 * to the hub, unless the viewer may not see the hub, when only its items are drawn) and the
 * site's own navigation under the site's name. `site` is `{ id, name }` or null outside a site;
 * `nav` is `{ global, hub, local }` as `GET /api/sites/{id}/navigation` answers. At narrow
 * widths the bars fold behind a Menu button. Labels and names are drawn as text only, and an
 * address only becomes a link after [`safeHref`] accepts it. `currentId` is marked as the
 * current page. With `vertical` (the site's look puts its navigation in a left column) the
 * view gets the class `vertical`; `hubThumbnail` is the hub's thumbnail address, shown beside
 * its name when it is a brand image.
 */
export function navigationView(site, nav, { currentId, vertical = false, hubThumbnail = null } = {}) {
  const bars = [];
  if (nav?.global?.length) bars.push(navBar('global', 'Organisation', null, nav.global, currentId));
  if (nav?.hub) {
    const hub = nav.hub;
    const head =
      hub.id != null && hub.name != null
        ? { name: hub.name, href: pageFor('site', hub.id), thumb: brandImageHref(hubThumbnail) }
        : null;
    if (head || hub.items?.length) bars.push(navBar('hub', 'Hub', head, hub.items ?? [], currentId));
  }
  if (site || nav?.local?.length) {
    const head = site ? { name: site.name, href: pageFor('site', site.id) } : null;
    bars.push(navBar('local', 'This site', head, nav?.local ?? [], currentId));
  }
  menus += 1;
  const id = `nav-menu-${menus}`;
  const menu = h('div', { class: 'chrome-menu', id }, ...bars);
  const toggle = h('button', { type: 'button', class: 'plain menu-toggle', 'aria-expanded': 'false', 'aria-controls': id }, 'Menu');
  toggle.addEventListener('click', () => {
    toggle.setAttribute('aria-expanded', toggle.getAttribute('aria-expanded') === 'true' ? 'false' : 'true');
  });
  const view = h('nav', { class: vertical ? 'chrome vertical' : 'chrome', 'aria-label': 'Site navigation', 'data-chrome': '' }, toggle, menu);
  view.hidden = bars.length === 0;
  // Escape closes what is open and goes back to the button that opened it.
  view.addEventListener('keydown', (event) => {
    if (event.key !== 'Escape') return;
    const open = [...(view.querySelectorAll?.('button[aria-expanded="true"]') ?? [])];
    const inner = open.filter((button) => button !== toggle);
    const last = inner.at(-1) ?? (open.includes(toggle) ? toggle : null);
    if (!last) return;
    if (last === toggle) toggle.setAttribute('aria-expanded', 'false');
    else setOpen(last, view.querySelector(`#${last.getAttribute('aria-controls')}`), false);
    last.focus();
  });
  return view;
}

let closing = null; // the document the outside-click closer listens on: one listener per page

/**
 * Closes every open sub-list of the navigation drawn on the page when the person clicks
 * elsewhere. The one listener looks the navigation up at each click, so it keeps working after
 * the bars are drawn again (joining or leaving a hub, saving navigation).
 */
function closeOnOutsideClick() {
  const doc = globalThis.document;
  if (!doc?.addEventListener || closing === doc) return;
  closing = doc;
  doc.addEventListener('click', (event) => {
    for (const view of doc.querySelectorAll?.('[data-chrome]') ?? []) {
      if (view.contains?.(event.target)) continue;
      for (const button of view.querySelectorAll?.('.sub-toggle[aria-expanded="true"]') ?? []) {
        setOpen(button, view.querySelector(`#${button.getAttribute('aria-controls')}`), false);
      }
    }
  });
}

/** Puts a drawn navigation in `place`, or after the page's account bar (replacing an earlier one). */
function placeChrome(view, place) {
  closeOnOutsideClick();
  if (place) return place.replaceChildren(view);
  const old = document.querySelector('[data-chrome]');
  if (old) return old.replaceWith(view);
  const bar = document.querySelector('[data-site-header]') ?? document.querySelector('.topbar, .bar');
  if (bar) bar.after(view);
  else document.body?.prepend(view);
}

// ---- the site's look: colours, header, footer, site cards and the editor frame bar ----

const BRAND_IMAGE = /^\/api\/brand\/images\/[0-9a-f]{64}$/;
const HEX_COLOUR = /^#[0-9a-fA-F]{6}$/;
const HEADER_LAYOUTS = ['minimal', 'compact', 'standard', 'extended'];
const BACKGROUNDS = ['none', 'soft', 'strong'];
const ALIGNS = ['left', 'centre', 'right'];
const FOOTER_LINKS = 8;
const FOOTER_GROUPS = 4;

/** `url` if it is the address of a brand image (`/api/brand/images/{sha256}`), else null. */
export function brandImageHref(url) {
  return typeof url === 'string' && BRAND_IMAGE.test(url) ? url : null;
}

const DRAFT_IMAGE = /^\/api\/docs\/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\/(preview|thumbnail)$/i;

/**
 * `url` if the preview of a draft look may load it, else null: a brand image, or the preview
 * or thumbnail of a document chosen in the picker but not yet applied (which only becomes a
 * brand image when the look is applied). Only Change the look's preview element uses this;
 * every page shows brand images only.
 */
export function draftImageHref(url) {
  return brandImageHref(url) ?? (typeof url === 'string' && DRAFT_IMAGE.test(url) ? url : null);
}

/**
 * A colour swatch for `colour` (`#rrggbb`), named `label` for its tooltip and screen readers:
 * the colour is set as `--swatch` through the CSS object model after it is checked, never
 * through markup. Anything else gives an empty, crossed swatch.
 */
export function swatch(colour, label) {
  const good = typeof colour === 'string' && HEX_COLOUR.test(colour) ? colour.toLowerCase() : null;
  const text = good ? `${label} ${good}` : `${label}: none`;
  const mark = h('span', { class: good ? 'swatch' : 'swatch none', title: text, role: 'img', 'aria-label': text });
  if (good) mark.style?.setProperty('--swatch', good);
  return mark;
}

/**
 * An `<img>` of the brand image `url` with `attributes`, hidden if it fails to load (a held
 * image answers 423); `onError` runs then too.
 */
function brandImage(url, attributes, onError) {
  const img = h('img', { ...attributes, src: url });
  img.addEventListener('error', () => {
    img.hidden = true;
    onError?.();
  });
  return img;
}

/**
 * A theme's four swatches (a theme as `GET /api/brand` lists it, with `resolved` colours when
 * it passes): primary and secondary in light mode, then in dark mode.
 */
export function themeSwatches(theme) {
  const light = theme?.resolved?.light;
  const dark = theme?.resolved?.dark;
  return h(
    'span',
    { class: 'swatches' },
    swatch(light?.accent ?? theme?.primary, 'Primary'),
    swatch(light?.header_bg ?? theme?.secondary, 'Secondary'),
    swatch(dark?.accent ?? theme?.primary_dark, 'Primary in dark mode'),
    swatch(dark?.header_bg ?? theme?.secondary_dark, 'Secondary in dark mode'),
  );
}

/**
 * The rows of the live contrast check for `check` (the answer of `checkTheme` in look.js): one
 * per failing pair, with its ratio and, where there is one, a swatch and a "Use suggested"
 * button that calls `onUse(pair)`. A theme whose colours are not all `#rrggbb` gets one row
 * saying so; a passing theme one row saying every pair reads.
 */
export function contrastRows(check, onUse) {
  if (check?.invalid) return [h('li', {}, h('span', { class: 'grow' }, 'Write each colour as # and six hexadecimal digits, like #3156c9.'))];
  if (check?.ok) return [h('li', { class: 'pass' }, h('span', { class: 'grow' }, `Every pair reads at ${MIN_RATIO}:1 or more, in light and dark mode.`))];
  return (check?.pairs ?? []).map((pair) => {
    const label = CHECK_LABELS[pair.check] ?? pair.check;
    const text = h(
      'span',
      { class: 'grow' },
      label,
      h('br'),
      h('small', { class: 'ratio' }, `${Number(pair.ratio).toFixed(2)}:1, needs ${MIN_RATIO}:1`),
    );
    if (typeof pair.suggestion !== 'string' || !parseColour(pair.suggestion)) {
      return h('li', {}, text, h('small', {}, 'Set this dark colour yourself.'));
    }
    const use = h('button', { type: 'button', class: 'plain', 'aria-label': `Use suggested ${pair.suggestion} for ${label}` }, `Use suggested ${pair.suggestion}`);
    use.addEventListener('click', () => onUse(pair));
    return h('li', {}, text, swatch(pair.suggestion, 'Suggested'), use);
  });
}

/**
 * A colour field: a label, a colour picker and a text box for `#rrggbb`, kept in step. `id` is
 * the text box's id; `optional` fields may be left empty (a dark colour then derived, shown as
 * the placeholder `hint`). `onInput(text)` runs on every change. Answers `{element, input,
 * set(value, hint)}`, `input` being the text box.
 */
export function colourField(id, label, { optional = false, onInput } = {}) {
  const text = h('input', { type: 'text', id, autocomplete: 'off', spellcheck: 'false', maxlength: '7', 'aria-describedby': `${id}-note` });
  const picker = h('input', { type: 'color', 'aria-label': `${label}, colour picker` });
  const note = h('small', { id: `${id}-note`, class: 'meta' }, optional ? 'Leave empty to work it out from the light colour.' : 'Like #3156c9.');
  text.addEventListener('input', () => {
    if (parseColour(text.value.trim())) picker.value = text.value.trim().toLowerCase();
    onInput?.(text.value);
  });
  picker.addEventListener('input', () => {
    text.value = picker.value;
    onInput?.(picker.value);
  });
  const set = (value, hint = '') => {
    text.value = value ?? '';
    text.placeholder = hint ? `Worked out: ${hint}` : '';
    const shown = parseColour(value ?? '') ? value : parseColour(hint) ? hint : '#000000';
    picker.value = shown.toLowerCase();
  };
  const element = h('div', { class: 'field' }, h('label', { for: id }, label), h('div', { class: 'colour-row' }, picker, text), note);
  return { element, input: text, set };
}

const oneOf = (value, choices, fallback) => (choices.includes(value) ? value : fallback);

/**
 * The header of a site page for the effective look `look` (`GET /api/sites/{id}/look`, or null
 * when it could not be read: today's compact header). `site` is `{ id, name }`. The layout,
 * background and alignment are data attributes the style sheet draws; the logo has the look's
 * `logo_alt`, or none when that only repeats the site name; the name is always there, hidden
 * for all but screen readers when the look hides the title (which needs a logo), and shown
 * again when the logo fails to load. An extended
 * header with an image shows it through `<img>` under a fixed dark shade. Names are text only,
 * and only brand images are loaded (with `draft`, in Change the look's preview, also the
 * documents [`draftImageHref`] allows).
 */
export function headerView(site, look, { draft = false } = {}) {
  const href = draft ? draftImageHref : brandImageHref;
  const header = look?.header ?? {};
  const layout = oneOf(header.layout, HEADER_LAYOUTS, 'compact');
  const extended = layout === 'extended';
  const imageUrl = extended && header.background === 'image' ? href(header.image_url) : null;
  const background = imageUrl ? 'image' : oneOf(header.background, BACKGROUNDS, 'none');
  const align = extended ? oneOf(header.align, ALIGNS, 'left') : 'left';
  const logoUrl = href(look?.logo_url);
  const hideTitle = Boolean(header.hide_title && logoUrl);
  const name = h('span', { class: hideTitle ? 'site-name sr' : 'site-name' }, site.name);
  // The name beside the logo names the link (shown, or for screen readers only), so a logo
  // described by the name alone is decoration: read once, not twice.
  const described = typeof look?.logo_alt === 'string' ? look.logo_alt.trim() : '';
  const alt = described === '' || described === String(site.name ?? '').trim() ? '' : described;
  const logo = logoUrl
    ? brandImage(logoUrl, { class: 'site-logo', alt }, () => name.setAttribute('class', 'site-name'))
    : null;
  const band = imageUrl
    ? [brandImage(imageUrl, { class: 'band-image', alt: '' }), h('span', { class: 'band-shade', 'aria-hidden': 'true' })]
    : [];
  return h(
    'header',
    { class: 'site-header', 'data-site-header': '', 'data-layout': layout, 'data-background': background, 'data-align': align },
    ...band,
    h('a', { class: 'site-brand', href: pageFor('site', site.id) }, logo, name),
  );
}

/**
 * The address a footer link opens: `https:`, `http:` or a path starting with a single `/`
 * (see [`safeHref`]), else null and the label is shown as text.
 */
function footerHref(url) {
  const href = safeHref(url);
  return href && !/^mailto:/i.test(href) ? href : null;
}

/** One footer link, opening in the same tab; one leaving ShareCraft is `noopener noreferrer`. */
function footerLink(link) {
  const href = footerHref(link?.url);
  const label = String(link?.label ?? '');
  if (!href) return h('li', {}, h('span', {}, label));
  const external = !href.startsWith('/');
  return h('li', {}, h('a', { href, rel: external ? 'noopener noreferrer' : null }, label));
}

/**
 * The footer of a site page for the effective look `look`, or null when it is off (or the look
 * could not be read). A simple footer has one group of at most 8 links and no heading; an
 * extended one up to 4 groups of 8 under their headings. Text only; links through
 * [`footerHref`]; only brand images are loaded (with `draft`, see [`headerView`]).
 */
export function footerView(look, { draft = false } = {}) {
  const footer = look?.footer;
  if (!footer?.enabled) return null;
  const layout = footer.layout === 'extended' ? 'extended' : 'simple';
  const background = oneOf(footer.background, BACKGROUNDS, 'none');
  const logoUrl = (draft ? draftImageHref : brandImageHref)(footer.logo_url);
  const name = typeof footer.name === 'string' ? footer.name : '';
  const logo = logoUrl ? brandImage(logoUrl, { class: 'footer-logo', alt: name ? '' : (look.logo_alt ?? '') }) : null;
  const groups = (Array.isArray(footer.groups) ? footer.groups : []).slice(0, layout === 'extended' ? FOOTER_GROUPS : 1);
  const lists = groups.map((group) => {
    const links = (Array.isArray(group?.links) ? group.links : []).slice(0, FOOTER_LINKS).map(footerLink);
    const heading = layout === 'extended' && group?.heading ? h('h2', { class: 'footer-heading' }, String(group.heading)) : null;
    return h('div', { class: 'footer-group' }, heading, links.length ? h('ul', { class: 'footer-links' }, ...links) : null);
  });
  return h(
    'footer',
    { class: 'site-footer', 'data-site-footer': '', 'data-layout': layout, 'data-background': background },
    logo || name ? h('p', { class: 'footer-brand' }, logo, name || null) : null,
    lists.length ? h('nav', { class: 'footer-groups', 'aria-label': 'Footer' }, ...lists) : null,
  );
}

/** Up to two initials of a site's name (the first letters of its first two words), or `?`. */
export function siteInitials(name) {
  const words = String(name ?? '').trim().split(/\s+/).filter(Boolean);
  const initials = words.slice(0, 2).map((word) => Array.from(word)[0].toLocaleUpperCase()).join('');
  return initials || '?';
}

/**
 * A site card's mark (a site as `GET /api/sites` lists it): its thumbnail, or its initials on
 * its accent, which is set as `--mark-accent` through the CSS object model after it is checked
 * to be `#rrggbb`. A thumbnail that fails to load gives way to the initials.
 */
export function siteMark(site) {
  const thumb = brandImageHref(site?.thumbnail_url);
  const initials = h('span', { class: 'site-initials' }, siteInitials(site?.name));
  const mark = h('span', { class: thumb ? 'site-mark' : 'site-mark initials', 'aria-hidden': 'true' }, initials);
  if (typeof site?.accent === 'string' && HEX_COLOUR.test(site.accent)) {
    mark.style?.setProperty('--mark-accent', site.accent);
  }
  if (thumb) {
    initials.hidden = true;
    mark.append(
      brandImage(thumb, { alt: '' }, () => {
        initials.hidden = false;
        mark.setAttribute('class', 'site-mark initials');
      }),
    );
  }
  return mark;
}

/**
 * The editor frame bar's mark (`.mono`): the site's thumbnail, or its logo, in place of the
 * letter; the letter comes back without either or when the picture fails to load.
 */
export function frameMark(mono, look) {
  if (!mono) return;
  const letter = () => {
    mono.replaceChildren('S');
    mono.setAttribute('class', 'mono');
  };
  const url = brandImageHref(look?.thumbnail_url) ?? brandImageHref(look?.logo_url);
  if (!url) return letter();
  mono.replaceChildren(brandImage(url, { alt: '' }, letter));
  mono.setAttribute('class', 'mono pic');
}

/** Puts `view` in place of the page's element matching `selector`, or after `after`; null removes it. */
function swapIn(selector, view, after) {
  const old = document.querySelector(selector);
  if (old && view) old.replaceWith(view);
  else if (old) old.remove();
  else if (view && after) after.after(view);
}

/**
 * Puts a site page's parts in the page around its account bar: the header after it, the
 * navigation after the header (or in `place`), the footer last; each replaces an earlier one.
 * A vertical navigation needs the rest of the page beside it, so what follows the navigation
 * (but the footer) is moved into `.side-main`, and moved back when the navigation is horizontal.
 */
function placeSite({ header, chrome, footer, vertical }, place) {
  const top = document.querySelector('.topbar');
  const page = top?.parentElement ?? null;
  swapIn('[data-site-header]', header, top);
  placeChrome(chrome, place);
  if (page && !place) sideBySide(page, chrome, vertical && !chrome.hidden);
  const old = document.querySelector('[data-site-footer]');
  if (old && footer) old.replaceWith(footer);
  else if (old) old.remove();
  else if (footer && page) page.append(footer);
}

/** Lays `page` out with the navigation `chrome` in a left column beside the rest, or not. */
function sideBySide(page, chrome, vertical) {
  let main = page.querySelector(':scope > .side-main');
  if (vertical) {
    if (!main) {
      const rest = [];
      for (let node = chrome.nextSibling; node; node = node.nextSibling) {
        if (!node.matches?.('[data-site-footer]')) rest.push(node);
      }
      main = h('div', { class: 'side-main' });
      chrome.after(main);
      main.append(...rest);
    }
    page.classList.add('nav-vertical');
  } else {
    if (main) main.replaceWith(...main.childNodes);
    page.classList.remove('nav-vertical');
  }
}

/**
 * The look, header and navigation of a page inside site `siteId`: fetches the site
 * (`GET /api/sites/{id}`), its navigation and its effective look once, sets the page's
 * language and time zone (and `<html lang>`) to the site's, applies the look's colours (see
 * `applyLook` in look.js), and draws the header after the account bar, the bars (see
 * [`navigationView`]) after it or in `place`, and with `footer` the site's footer at the end.
 * In the editor frame (`frame`) there is no header or footer, and only the bar and the
 * navigation take the colours (not `<html>`, so the editor stays untouched): the bar's mark
 * shows the site's thumbnail or logo instead (see [`frameMark`]), and the navigation stays
 * across the top.
 * `currentId` is marked as the current page. Answers `{ site, navigation, look }`, each null
 * when it could not be read; without the site the organisation defaults are used, and without
 * the look the page keeps its own.
 */
export async function siteChrome(siteId, { currentId, place, footer = false, frame = false } = {}) {
  const [site, navigation, look] = await Promise.all([
    api('GET', `/sites/${seg(siteId)}`),
    api('GET', `/sites/${seg(siteId)}/navigation`),
    api('GET', `/sites/${seg(siteId)}/look`),
  ]);
  if (site.ok) setRegional({ language: site.data.language, timeZone: site.data.time_zone });
  else await useOrganisationRegional();
  const shown = {
    site: site.ok ? site.data : null,
    navigation: navigation.ok ? navigation.data : null,
    look: look.ok && look.data && typeof look.data === 'object' ? look.data : null,
  };
  // In the editor frame only the bar and the navigation take the look: the editor stays neutral.
  if (!frame) applyLook(document.documentElement, shown.look);
  const vertical = !frame && shown.look?.navigation === 'vertical';
  const view = navigationView(shown.site, shown.navigation, {
    currentId,
    vertical,
    hubThumbnail: shown.look?.hub?.thumbnail_url,
  });
  if (frame) {
    applyLook(document.querySelector('.bar'), shown.look);
    applyLook(view, shown.look);
    frameMark(document.querySelector('.bar .mono'), shown.look);
    placeChrome(view, place);
  } else {
    placeSite(
      {
        header: shown.site ? headerView(shown.site, shown.look) : null,
        chrome: view,
        footer: footer ? footerView(shown.look) : null,
        vertical,
      },
      place,
    );
  }
  return shown;
}

/** Sets the page's language and time zone to the organisation defaults. */
async function useOrganisationRegional() {
  const result = await api('GET', '/settings/regional');
  if (result.ok) setRegional({ language: result.data.default_language, timeZone: result.data.default_time_zone });
}

/**
 * The navigation of a page outside any site: the organisation's language and time zone and its
 * global bar (`GET /api/navigation`), drawn after the account bar or in `place`.
 */
export async function organisationChrome({ place } = {}) {
  const [, navigation] = await Promise.all([useOrganisationRegional(), api('GET', '/navigation')]);
  const global = navigation.ok ? navigation.data.items : [];
  placeChrome(navigationView(null, { global, hub: null, local: [] }), place);
}

/** `Team site` or `Communication site`. */
export const kindLabel = (kind) => (kind === 'communication' ? 'Communication site' : 'Team site');

/**
 * The sites of `sites` (directory rows) whose name or description holds `text`, ignoring case
 * and spaces at either end; every site for no text.
 */
export function directoryFilter(sites, text) {
  const needle = String(text ?? '').trim().toLocaleLowerCase();
  if (needle === '') return sites;
  return sites.filter(
    (site) =>
      String(site.name ?? '').toLocaleLowerCase().includes(needle) ||
      String(site.description ?? '').toLocaleLowerCase().includes(needle),
  );
}

/**
 * One site of the directory (as `GET /api/sites` lists it) as a list row: its mark (see
 * [`siteMark`]), its name linked, its kind, whether it is a hub or archived, the hub it
 * joined, its description and the person's role.
 */
export function directoryRow(site) {
  return h(
    'li',
    {},
    siteMark(site),
    h(
      'span',
      { class: 'grow' },
      h('a', { href: pageFor('site', site.id) }, site.name),
      h('br'),
      h('small', {}, kindLabel(site.kind)),
      site.hub ? h('small', {}, ' · Hub: ', h('a', { href: pageFor('site', site.hub.id) }, site.hub.name)) : null,
      site.description ? h('br') : null,
      site.description ? h('span', { class: 'description' }, site.description) : null,
    ),
    site.is_hub ? h('small', { class: 'mark own' }, 'Hub') : null,
    site.archived ? h('small', { class: 'mark' }, 'Archived') : null,
    site.access ? h('small', {}, roleLine(site.access)) : h('small', { class: 'mark' }, 'Limited access'),
  );
}

// ---- site settings, templates and the administrators' Sites page ----

/** The languages a site may be set to, as `[tag, name]`: a fixed list of common ones. */
export const LANGUAGES = Object.freeze([
  ['en', 'English'],
  ['en-GB', 'English (United Kingdom)'],
  ['en-US', 'English (United States)'],
  ['ar', 'Arabic'],
  ['bg', 'Bulgarian'],
  ['ca', 'Catalan'],
  ['cs', 'Czech'],
  ['cy', 'Welsh'],
  ['da', 'Danish'],
  ['de', 'German'],
  ['de-CH', 'German (Switzerland)'],
  ['el', 'Greek'],
  ['es', 'Spanish'],
  ['es-MX', 'Spanish (Mexico)'],
  ['et', 'Estonian'],
  ['fi', 'Finnish'],
  ['fr', 'French'],
  ['fr-CA', 'French (Canada)'],
  ['ga', 'Irish'],
  ['he', 'Hebrew'],
  ['hi', 'Hindi'],
  ['hr', 'Croatian'],
  ['hu', 'Hungarian'],
  ['id', 'Indonesian'],
  ['is', 'Icelandic'],
  ['it', 'Italian'],
  ['ja', 'Japanese'],
  ['ko', 'Korean'],
  ['lt', 'Lithuanian'],
  ['lv', 'Latvian'],
  ['ms', 'Malay'],
  ['nb', 'Norwegian Bokmål'],
  ['nl', 'Dutch'],
  ['pl', 'Polish'],
  ['pt', 'Portuguese'],
  ['pt-BR', 'Portuguese (Brazil)'],
  ['ro', 'Romanian'],
  ['ru', 'Russian'],
  ['sk', 'Slovak'],
  ['sl', 'Slovenian'],
  ['sr', 'Serbian'],
  ['sv', 'Swedish'],
  ['th', 'Thai'],
  ['tr', 'Turkish'],
  ['uk', 'Ukrainian'],
  ['vi', 'Vietnamese'],
  ['zh-Hans', 'Chinese (Simplified)'],
  ['zh-Hant', 'Chinese (Traditional)'],
]);

/**
 * The language choices of a select, `[{value, label}]`: `current` first when it is not in the
 * fixed list (a site set through the API keeps its language), then the list.
 */
export function languageChoices(current) {
  const listed = LANGUAGES.map(([value, name]) => ({ value, label: `${name} (${value})` }));
  if (current && !LANGUAGES.some(([value]) => value === current)) return [{ value: current, label: current }, ...listed];
  return listed;
}

/** The time zones `Intl` knows, `UTC` first, with `current` kept when `Intl` does not list it. */
export function timeZoneChoices(current) {
  let zones = [];
  try {
    zones = Intl.supportedValuesOf('timeZone');
  } catch {
    zones = [];
  }
  const all = ['UTC', ...zones.filter((zone) => zone !== 'UTC')];
  if (current && !all.includes(current)) all.splice(1, 0, current);
  return all;
}

/** Fills `select` with `choices` (`[{value, label}]` or strings) and chooses `current`. Text only. */
export function fillSelect(select, choices, current) {
  select.replaceChildren(
    ...choices.map((choice) =>
      typeof choice === 'string' ? h('option', { value: choice }, choice) : h('option', { value: choice.value }, choice.label),
    ),
  );
  select.value = current ?? '';
}

const TEMPLATE_REASONS = Object.assign(Object.create(null), {
  too_large: 'The package is larger than 1 MB.',
  not_utf8: 'The package is not a text file.',
  unreadable: 'The file is not a template package: it could not be read as one.',
  unknown_version: 'The package was made by a newer version of ShareCraft.',
  identifiers: 'The package names things by identifier. A package names libraries by name only.',
  unknown_field: 'The package holds something a template cannot carry.',
  invalid_name: 'The template name is empty or longer than 80 characters.',
  invalid_description: 'The description is longer than 500 characters.',
  invalid_kind: 'The package makes a kind of site that does not exist.',
  invalid_language: 'The package has a language that is not a language code.',
  invalid_time_zone: 'The package has a time zone that is not known.',
  too_many_libraries: 'The template has more than 100 libraries.',
  too_many_folders: 'The template has more than 5,000 folders.',
  too_deep: 'The template has folders more than 20 levels deep.',
  invalid_names: 'The template has a library or folder name that is not allowed, or the same name twice in one place.',
  invalid_navigation: 'The template has a navigation link that is not allowed.',
});

/** The sentence for the `reason` of a refused template (`422 invalid_template`). */
export function templateReasonText(reason) {
  return TEMPLATE_REASONS[reason] ?? messageFor('invalid_template');
}

/** The sentence for a refused template answer from `api`: its reason when there is one. */
export function templateRefusal(result) {
  if (result.error === 'invalid_template') return templateReasonText(result.data?.reason);
  if (result.status === 413) return TEMPLATE_REASONS.too_large;
  return messageFor(result.error);
}

/** Asking before deleting the library `name`. */
export const deleteLibraryText = (name) =>
  `Delete the library ${name}? It goes to the recycle bin with everything in it, and can be restored from there.`;

/** Asking before archiving `site` (a row of the administrators' Sites page). */
export const archiveSiteText = (site) =>
  `Archive the site ${site.name}? It becomes read-only for everyone and is hidden from the site directory until an administrator reactivates it.`;

/** Asking before deleting `site`. */
export const deleteSiteText = (site) =>
  `Delete the site ${site.name}? Everyone loses access at once. It can be restored for 93 days.`;

/** Asking before purging the deleted `site`: it names the site and says it cannot be undone. */
export const purgeSiteText = (site) =>
  `Delete the site ${site.name} permanently? Everything in it is erased now and it can no longer be restored. This cannot be undone.`;

/** The actions the administrators' Sites page offers for `site`, by its state. */
export function adminSiteActions(site) {
  switch (site.state) {
    case 'deleted':
      return [
        { action: 'restore', label: 'Restore', name: `Restore ${site.name}` },
        { action: 'purge', label: 'Delete permanently', name: `Delete ${site.name} permanently` },
      ];
    case 'archived':
      return [
        { action: 'reactivate', label: 'Reactivate', name: `Reactivate ${site.name}` },
        { action: 'delete', label: 'Delete', name: `Delete ${site.name}` },
      ];
    default:
      return [
        { action: 'archive', label: 'Archive', name: `Archive ${site.name}` },
        { action: 'delete', label: 'Delete', name: `Delete ${site.name}` },
      ];
  }
}

const STATE_LABELS = { active: 'Active', archived: 'Archived', deleted: 'Deleted' };

/**
 * One site of `GET /api/admin/sites` as a list row: its name (linked unless deleted), state,
 * kind, hub, storage and owners (flagged when it has no enabled owner), when it was deleted and
 * when it will be erased, and its action buttons, which call `onAction(action, site)`. Text only.
 */
export function adminSiteRow(site, onAction) {
  const storage =
    site.quota == null ? `${formatSize(site.used)} used, no quota` : `${formatSize(site.used)} of ${formatSize(site.quota)} used`;
  const owners = site.owners > 0 ? `${site.owners} owner${site.owners === 1 ? '' : 's'}` : null;
  const buttons = adminSiteActions(site).map(({ action, label, name }) => {
    const button = h('button', { type: 'button', class: 'plain', 'aria-label': name }, label);
    button.addEventListener('click', () => onAction(action, site));
    return button;
  });
  return h(
    'li',
    { class: 'admin-site' },
    h(
      'span',
      { class: 'grow' },
      site.state === 'deleted' ? h('strong', {}, site.name) : h('a', { href: pageFor('site', site.id) }, site.name),
      h('br'),
      h('small', {}, `${STATE_LABELS[site.state] ?? site.state} · ${kindLabel(site.kind)}`),
      site.hub ? h('small', {}, ' · In the hub ', site.hub.name) : null,
      h('br'),
      h('small', {}, storage),
      owners ? h('small', {}, ` · ${owners}`) : null,
      site.deleted ? h('br') : null,
      site.deleted
        ? h(
            'small',
            {},
            `Deleted ${formatDate(site.deleted.at)}${site.deleted.by ? ` by ${site.deleted.by}` : ''}. Erased for good ${formatDate(site.purge_at)} unless restored.`,
          )
        : null,
    ),
    site.is_hub ? h('small', { class: 'mark own' }, 'Hub') : null,
    site.owners > 0 ? null : h('small', { class: 'mark warn' }, 'No enabled owner'),
    h('span', { class: 'actions' }, ...buttons),
  );
}

// ---- libraries: kinds and settings, previews, scanning and quarantine ----

/** What a file page shows while the malware scanner holds a file. */
export const HELD_TEXT = 'Being checked for malware';

/**
 * What the quarantine page says after a delete whose content the brand still uses
 * (`pinned_by_brand` of `POST /api/admin/quarantine/{sha256}/delete`: `{owner: 'org'}` or
 * `{owner: 'site', id, name}`), or null when nothing does. Plain text; names are put in as text.
 */
export function brandPinsText(pins) {
  if (!Array.isArray(pins) || pins.length === 0) return null;
  const users = pins.map((pin) => (pin?.owner === 'org' ? 'the brand centre' : `the look of site ${pin?.name ?? ''}`));
  const listed =
    users.length === 1 ? `by ${users[0]}` : `${users.slice(0, -1).map((u) => `by ${u}`).join(', ')} and by ${users.at(-1)}`;
  return `Still used ${listed}; change the brand to remove it. Until then it stays held and is never shown.`;
}

/** The confirmation before an administrator deletes quarantined content. */
export const QUARANTINE_DELETE_TEXT =
  'Delete every file holding this content? They skip the recycle bin and cannot be restored.';

// The server's preview list (crates/server/src/previews.rs), by extension; anything else has none.
const PREVIEW_KINDS = Object.freeze({
  png: 'image', jpg: 'image', jpeg: 'image', gif: 'image', webp: 'image', avif: 'image', bmp: 'image', svg: 'image',
  mp3: 'audio', ogg: 'audio', wav: 'audio', m4a: 'audio', flac: 'audio',
  mp4: 'video', webm: 'video', mov: 'video',
});
const TEXT_PREVIEWS = new Set([
  'txt', 'md', 'csv', 'tsv', 'log', 'json', 'xml', 'yaml', 'yml', 'toml', 'ini', 'rs', 'js', 'ts', 'py', 'java', 'c',
  'h', 'cpp', 'cs', 'go', 'rb', 'php', 'sh', 'ps1', 'sql', 'css', 'html', 'htm',
]);

/**
 * How a file named `name` is previewed, as the server decides it by its extension (what follows
 * the last dot, with something before it): `image`, `audio`, `video`, `text`, or null for none.
 * Web pages are text: they are only ever shown as text.
 */
export function previewKindOf(name) {
  const text = String(name ?? '');
  const dot = text.lastIndexOf('.');
  if (dot <= 0 || dot === text.length - 1) return null;
  const ext = text.slice(dot + 1).toLowerCase();
  if (Object.hasOwn(PREVIEW_KINDS, ext)) return PREVIEW_KINDS[ext];
  return TEXT_PREVIEWS.has(ext) ? 'text' : null;
}

/**
 * What the file page shows for `doc` as `GET /api/docs/{id}` answers it: `{view: 'quarantined',
 * threat}`, `{view: 'held'}`, `{view: 'editor'}` for an editor's file, `{view: 'preview', kind}`
 * for any other file a browser can show safely, or `{view: 'download'}`.
 */
export function fileView(doc) {
  if (doc.scan === 'quarantined') return { view: 'quarantined', threat: doc.threat ?? null };
  if (doc.scan === 'held') return { view: 'held' };
  if (doc.editor !== 'other') return { view: 'editor' };
  const kind = doc.revision == null ? null : previewKindOf(doc.name);
  return kind ? { view: 'preview', kind } : { view: 'download' };
}

/**
 * Waits for a held file: every `every` milliseconds (10 seconds) it calls `check`, at most
 * `times` (30) times. Resolves with the first answer `check` gives that is not null, or null
 * when it gives up or `stop()` says the page no longer wants it.
 */
export async function pollHeld(check, { wait = sleep, every = 10_000, times = 30, stop = () => false } = {}) {
  for (let tried = 0; tried < times; tried += 1) {
    await wait(every);
    if (stop()) return null;
    const answer = await check();
    if (answer != null) return answer;
    if (stop()) return null;
  }
  return null;
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/** The sentence for a quarantined file, naming the threat (put in as text by the callers). */
export function quarantineText(threat) {
  const found = threat ? `The malware scanner found ${threat} in this file.` : 'The malware scanner found a threat in this file.';
  return `${found} It is quarantined: nobody can open or download it. An administrator can release or delete it.`;
}

/** A paragraph saying that a file is quarantined, with the threat's name as text. */
export function quarantineNotice(threat) {
  const rest = ' It is quarantined: nobody can open or download it. An administrator can release or delete it.';
  if (!threat) return h('p', { class: 'note' }, quarantineText(null));
  return h('p', { class: 'note' }, 'The malware scanner found ', h('strong', {}, threat), ` in this file.${rest}`);
}

const HELD_REASONS = Object.freeze({
  infected: 'Threat found',
  failed: 'The scanner could not check it',
  too_large: 'Larger than the scanner takes',
  pending: 'Waiting for the scanner',
});

/**
 * A row of the administrators' quarantine list for `entry` (`GET /api/admin/quarantine`): the
 * threat or why it is held, its size, each document's path, and Release and Delete buttons that
 * call `onAction(action, entry)`. Everything from the server is text.
 */
export function quarantineRow(entry, onAction) {
  const reason = HELD_REASONS[entry.scan] ?? 'Held';
  const release = h('button', { type: 'button', class: 'plain', 'aria-label': `Release ${entry.threat ?? 'this content'}` }, 'Release');
  const remove = h('button', { type: 'button', class: 'plain', 'aria-label': `Delete ${entry.threat ?? 'this content'}` }, 'Delete');
  release.addEventListener('click', () => onAction('release', entry));
  remove.addEventListener('click', () => onAction('delete', entry));
  const documents = entry.documents ?? [];
  return h(
    'li',
    { class: 'quarantine' },
    h(
      'span',
      { class: 'grow' },
      h('strong', {}, entry.threat ?? reason),
      h('br'),
      h('small', {}, `${reason} · ${formatSize(entry.size)}${entry.scanned_at ? ` · checked ${formatDate(entry.scanned_at)}` : ''}`),
      documents.length
        ? h(
            'ul',
            { class: 'paths' },
            ...documents.map((doc) =>
              h('li', {}, doc.path, doc.deleted ? h('small', {}, ' (in the recycle bin)') : null),
            ),
          )
        : h('small', { class: 'meta' }, ' No file names it now.'),
    ),
    h('span', { class: 'actions' }, release, remove),
  );
}

const EXTENSION = /^\.[a-z0-9]+(\.[a-z0-9]+)*$/;

/**
 * The file types typed in a comma-separated field, normalised as the server keeps them: trimmed,
 * lower case, with a leading dot, once each. `{types}`, or `{error}` for one that is not 2 to 20
 * letters, digits and dots, or more than 100.
 */
export function parseTypeList(text) {
  const types = [];
  for (const part of String(text ?? '').split(',')) {
    let type = part.trim().toLowerCase();
    if (type === '') continue;
    if (!type.startsWith('.')) type = `.${type}`;
    if (type.length < 2 || type.length > 20 || !EXTENSION.test(type)) {
      return { error: `“${part.trim()}” is not a file type. Write types like .png or .tar.gz, separated by commas.` };
    }
    if (!types.includes(type)) types.push(type);
  }
  if (types.length > 100) return { error: 'List at most 100 file types.' };
  return { types };
}

/** A list of file types as the comma-separated field shows it. */
export const typeListText = (types) => (Array.isArray(types) ? types.join(', ') : '');

/** The largest-file field (megabytes, decimals allowed): `{bytes}`, null when empty, or `{error}`. */
export function parseMegabytes(text) {
  const trimmed = String(text ?? '').trim();
  if (trimmed === '') return { bytes: null };
  if (!/^\d+(\.\d+)?$/.test(trimmed) || Number(trimmed) <= 0) {
    return { error: 'Enter the largest file as a number of megabytes, or leave it empty for no limit.' };
  }
  return { bytes: Math.round(Number(trimmed) * 1024 * 1024) };
}

/** A byte count as the megabytes field shows it; '' for none. */
export const megabytesText = (bytes) =>
  typeof bytes === 'number' ? String(Math.round((bytes / (1024 * 1024)) * 100) / 100) : '';

const LIBRARY_KINDS = Object.freeze([
  { value: 'document', label: 'Document library' },
  { value: 'image', label: 'Image library' },
  { value: 'template', label: 'Template library' },
  { value: 'asset', label: 'Asset library' },
]);

/** The name of a library kind. */
export const libraryKindLabel = (kind) => LIBRARY_KINDS.find((k) => k.value === kind)?.label ?? 'Library';

/** The kinds a person may choose: asset only for administrators. */
export const libraryKindChoices = (isAdmin) => LIBRARY_KINDS.filter((k) => isAdmin || k.value !== 'asset');

/** The views a library of `kind` offers: image libraries have a grid of thumbnails too. */
export const libraryViews = (kind) => (kind === 'image' ? ['list', 'grid'] : ['list']);

/** The marks a listed item carries: held, quarantined (with its threat) and blocked. */
export function itemMarks(item) {
  const marks = [];
  if (item.scan === 'held') marks.push(HELD_TEXT);
  if (item.scan === 'quarantined') marks.push(item.threat ? `Quarantined: ${item.threat}` : 'Quarantined');
  if (item.policy === 'blocked') marks.push('Type not allowed here');
  return marks;
}

/**
 * A thumbnail from `url` as a Blob, or null when there is none (415, any refusal, or no
 * connection). A busy server (503) is asked again after 1, 2, 4 and 8 seconds before giving up.
 */
export async function loadThumbnail(url, { fetch = globalThis.fetch, wait = sleep, tries = 5, first = 1000 } = {}) {
  let delay = first;
  for (let tried = 1; ; tried += 1) {
    let response;
    try {
      response = await fetch(url, { credentials: 'same-origin' });
    } catch {
      return null;
    }
    if (response.ok) return response.blob();
    if (response.status !== 503 || tried >= tries) return null;
    await wait(delay);
    delay *= 2;
  }
}

/**
 * Shows in `picture` (a tile's thumbnail box) the thumbnail at `url` as an image that loads
 * the route itself: the pages' policy takes images from their own origin only, so a `blob:`
 * URL would show as a broken image. Should the image fail, the route is asked once more through
 * `load` ({@link loadThumbnail}), which waits out a busy server: the image again when it then
 * answers, the plain placeholder otherwise (or what `fallback()` makes, such as a file's type
 * icon). Nothing is drawn into a box no longer in the page.
 * `doc` makes the elements (the page's document; a test's stand-in).
 */
export function showThumbnail(picture, url, { load = loadThumbnail, doc = globalThis.document, fallback = null } = {}) {
  const show = (retry) => {
    const img = doc.createElement('img');
    img.setAttribute('alt', '');
    img.addEventListener(
      'error',
      async () => {
        if (!picture.isConnected) return;
        const again = retry && (await load(url)) !== null;
        if (!picture.isConnected) return;
        if (again) {
          show(false);
          return;
        }
        if (fallback) {
          picture.replaceChildren(fallback());
          return;
        }
        const none = doc.createElement('span');
        none.setAttribute('class', 'placeholder');
        none.textContent = 'No thumbnail';
        picture.replaceChildren(none);
      },
      { once: true },
    );
    img.setAttribute('src', url);
    picture.replaceChildren(img);
    return img;
  };
  return show(true);
}

/**
 * The sentence for a file refused by the file policies, naming it and what was refused:
 * `blocked_file_type {extension}` or `file_too_large {limit}`; null for any other error.
 */
export function policyRefusalText(error, data, name) {
  if (error === 'blocked_file_type') {
    const type = data?.extension ? `Files of type ${data.extension}` : 'Files of that type';
    return `“${name}” was not accepted. ${type} are not allowed here: the library or the organisation blocks them.`;
  }
  if (error === 'file_too_large') {
    const limit = typeof data?.limit === 'number' ? ` (at most ${formatSize(data.limit)})` : '';
    return `“${name}” is larger than this library takes${limit}. Choose a smaller file or another library.`;
  }
  return null;
}

const LIBRARY_REASONS = Object.assign(Object.create(null), {
  invalid_kind: 'That is not a kind of library.',
  invalid_description: 'The description is too long. Use 500 characters or fewer.',
  too_many_types: 'List at most 100 file types.',
  invalid_extension: 'A file type is written like .png or .tar.gz: 2 to 20 letters, digits and dots.',
  duplicate_type: 'A file type is listed twice.',
  no_types: 'List at least one allowed type, or leave the field empty to allow every type.',
  not_an_image_type: 'An image library accepts only image types (.png .jpg .jpeg .gif .webp .svg .avif .bmp .tif .tiff .heic).',
  invalid_max_file_bytes: 'The largest file must be more than 0 MB.',
});

/** The sentence for an `invalid_library_settings` refusal's `reason`. */
export function libraryReasonText(reason) {
  return LIBRARY_REASONS[reason] ?? messageFor('invalid_library_settings');
}
