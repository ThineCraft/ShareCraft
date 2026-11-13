// Who can reach a site, library, folder or file, people and groups, and whether that comes from
// above. For those who manage access; the server decides, and refusals are shown as sentences.

import {
  ANYONE_NOTICE,
  api,
  can,
  expiryFromDays,
  expiryText,
  groupLabel,
  h,
  inheritAgainMessage,
  linkLabel,
  linkRevokeHint,
  linkRoleChoices,
  liveLinks,
  messageFor,
  offerRequest,
  permsText,
  previewView,
  principalParts,
  queryId,
  requireMe,
  rolesWithin,
  seg,
  setupTopbar,
  showCrumbs,
  showMessage,
} from './common.js';

const $ = (id) => document.getElementById(id);
const error = $('error');
const MISSING = 'This item does not exist, or you do not have access to it.';

const me = await requireMe();
setupTopbar(me, error);
const nodeId = queryId(location.search);
if (!nodeId) fail(MISSING);
const here = await showCrumbs($('crumbs'), nodeId);
if (!here) {
  offerRequest(error, nodeId);
  fail(MISSING);
}
const self = here.path[here.path.length - 1];
$('title').textContent = `Access to ${self.name}`;
document.title = `Access to ${self.name} — ShareCraft`;
if (!can(here.access, 'manage_access')) fail('Only people who manage access here can see or change who has access.');
$('explorer-link').href = `/explorer.html?root=${seg(nodeId)}`;

let state = null; // { inherits, source, assignments } as the server last said
let roles = []; // every role, built-in first, as GET /api/roles lists them
let groups = []; // the groups this person may see, as GET /api/groups lists them

const rolesResult = await api('GET', '/roles');
if (rolesResult.ok) roles = rolesResult.data;
const groupsResult = await api('GET', '/groups');
if (groupsResult.ok) groups = groupsResult.data;
fillRoleSelect($('share-role'), 'editor');
fillRoleSelect($('group-role'), 'editor');
fillGroupSelect();

// What links may be made here: anyone links only while an administrator allows them.
let sharing = { anyone_links: false, anyone_link_max_days: 30 };
const sharingResult = await api('GET', '/settings/sharing');
if (sharingResult.ok) sharing = sharingResult.data;
if (!sharing.anyone_links) $('anyone-option').remove();
else $('anyone-option').hidden = false;
let links = []; // the live links of this item, as GET /api/nodes/{id}/links lists them
let revoking = null;

$('inherit-cancel').addEventListener('click', () => $('inherit-dialog').close());
fillLinkForm();
await refresh();
await refreshLinks();

function note(text, isError) {
  error.classList.toggle('ok', !isError);
  showMessage(error, text);
}

/** Puts every role in `select` (the one with id `selected` chosen). */
function fillRoleSelect(select, selected, extra) {
  // Only roles this person may give; an entry's current role stays visible in its own select.
  const allowed = rolesWithin(roles, here.access);
  const list = extra && !allowed.some((role) => role.id === extra.id) ? [...allowed, extra] : allowed;
  select.replaceChildren(
    ...list.map((role) => {
      const option = h('option', { value: role.id }, role.name);
      option.selected = role.id === selected;
      return option;
    }),
  );
}

function fillGroupSelect() {
  $('group-form').hidden = groups.length === 0;
  $('group-pick').replaceChildren(
    ...groups.map((group) => h('option', { value: group.id }, groupLabel(group))),
  );
}

async function refresh() {
  const result = await api('GET', `/nodes/${seg(nodeId)}/access`);
  if (!result.ok) {
    $('content').hidden = true;
    note(result.status === 404 ? MISSING : messageFor(result.error), true);
    if (result.status === 404) offerRequest(error, nodeId);
    return;
  }
  state = result.data;
  show();
}

function show() {
  $('content').hidden = false;
  const isSite = self.kind === 'site';
  const source = $('source');
  if (state.inherits) {
    source.replaceChildren(
      'This item inherits its access from ',
      h('a', { href: `/access.html?id=${seg(state.source.id)}` }, state.source.name),
      '. The people and groups below are listed there.',
    );
  } else if (isSite) {
    source.textContent = 'A site has its own list of people and groups. It cannot inherit.';
  } else {
    source.textContent = 'This item has its own list of people and groups. Changes above do not reach it.';
  }
  $('inherit-note').textContent = state.inherits
    ? 'Stopping inheritance copies the entries below onto this item, so you can change them here without changing the place above.'
    : '';
  const toggle = $('inherit-toggle');
  toggle.hidden = isSite;
  toggle.textContent = state.inherits ? 'Stop inheriting' : 'Inherit again';
  $('share-hint').hidden = !state.inherits;
  $('no-assignments').hidden = state.assignments.length > 0;
  $('assignments').replaceChildren(...state.assignments.map(assignmentRow));
}

// Where a link is revoked: here, or (on a break below holding a copy) on its own item's page.
function revokeHint(principal) {
  const { text, on } = linkRevokeHint(principal, nodeId);
  if (!on) return h('small', { class: 'meta' }, text);
  return h('small', { class: 'meta' }, `${text} on `, h('a', { href: on.href }, on.name));
}

function assignmentRow({ principal, role, perms }) {
  if (principal.kind === 'link') {
    // A link is changed under "Share by link", never as a person or group.
    return h(
      'li',
      {},
      h(
        'span',
        { class: 'grow' },
        principalParts(principal, role).name,
        h('br'),
        h('small', {}, `Can ${permsText(perms)}`),
      ),
      revokeHint(principal),
    );
  }
  const { name, note: sub, hidden } = principalParts(principal);
  const isUser = principal.kind === 'user';
  const target = isUser ? { email: principal.email } : { group: principal.id };
  let roleControl;
  if (hidden || (isUser && !principal.email)) {
    roleControl = h('span', {}, role.name);
  } else {
    roleControl = h('select', { 'aria-label': `Role of ${name}` });
    fillRoleSelect(roleControl, role.id, role);
    roleControl.addEventListener('change', () => setRole(target, roleControl.value));
  }
  const remove = h('button', { type: 'button', class: 'plain', 'aria-label': `Remove ${name}` }, 'Remove');
  remove.addEventListener('click', () => removeEntry(principal, name));
  return h(
    'li',
    {},
    h(
      'span',
      { class: 'grow' },
      name,
      sub ? h('br') : null,
      sub ? h('small', {}, sub) : null,
      h('br'),
      h('small', {}, `Can ${permsText(perms)}`),
    ),
    roleControl,
    remove,
  );
}

async function setRole(target, role) {
  note('', false);
  const result = await api('PUT', `/nodes/${seg(nodeId)}/access`, { ...target, role });
  note(result.ok ? 'Saved.' : messageFor(result.error), !result.ok);
  await refresh();
}

async function removeEntry(principal, name) {
  note('', false);
  const result = await api('DELETE', `/nodes/${seg(nodeId)}/access/${seg(principal.kind)}/${seg(principal.id)}`);
  note(result.ok ? `${name} was removed.` : messageFor(result.error), !result.ok);
  await refresh();
}

$('share-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  note('', false);
  const result = await api('PUT', `/nodes/${seg(nodeId)}/access`, {
    email: $('share-email').value.trim(),
    role: $('share-role').value,
  });
  if (result.ok) $('share-email').value = '';
  note(result.ok ? 'Added.' : messageFor(result.error), !result.ok);
  await refresh();
});

$('group-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  note('', false);
  const result = await api('PUT', `/nodes/${seg(nodeId)}/access`, {
    group: $('group-pick').value,
    role: $('group-role').value,
  });
  note(result.ok ? 'Added.' : messageFor(result.error), !result.ok);
  await refresh();
});

$('inherit-toggle').addEventListener('click', async () => {
  note('', false);
  if (state.inherits) {
    const result = await api('POST', `/nodes/${seg(nodeId)}/inheritance`, { inherit: false });
    note(result.ok ? 'This item now has its own list of people and groups.' : messageFor(result.error), !result.ok);
    await refresh();
    return;
  }
  const result = await api('GET', `/nodes/${seg(nodeId)}/inheritance/preview`);
  if (!result.ok) return note(inheritAgainMessage(result.error), true);
  $('inherit-text').replaceChildren(
    h('p', {}, `Inheriting again removes the entries of its own on “${self.name}”.`),
    previewView(result.data),
  );
  showMessage($('inherit-error'), '');
  $('inherit-dialog').showModal();
});

$('inherit-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  const result = await api('POST', `/nodes/${seg(nodeId)}/inheritance`, { inherit: true });
  if (!result.ok) return showMessage($('inherit-error'), inheritAgainMessage(result.error));
  $('inherit-dialog').close();
  note('This item inherits its access again.', false);
  await refresh();
});

// ---- share by link ----

/** The role choices and expiry hint for the kind of link chosen. */
function fillLinkForm() {
  const anyone = $('link-kind').value === 'anyone';
  const select = $('link-role');
  if (anyone) {
    select.replaceChildren(h('option', { value: 'viewer' }, 'Viewer'));
    select.disabled = true;
    $('link-days-label').textContent = 'Expires after (days, required)';
    $('link-hint').textContent = `A link for anyone can only view and download, and lasts at most ${sharing.anyone_link_max_days} days.`;
  } else {
    const choices = linkRoleChoices(roles, here.access);
    select.replaceChildren(
      ...choices.map((role) => {
        const option = h('option', { value: role.id }, role.name);
        option.selected = role.id === 'viewer';
        return option;
      }),
    );
    select.disabled = false;
    $('link-days-label').textContent = 'Expires after (days, optional)';
    $('link-hint').textContent =
      'Leave the days empty for a link that does not expire. A link never makes anyone a manager. Making one gives this item its own list of people if it inherited.';
  }
}

$('link-kind').addEventListener('change', fillLinkForm);

async function refreshLinks() {
  const result = await api('GET', `/nodes/${seg(nodeId)}/links`);
  if (!result.ok) return note(messageFor(result.error), true);
  links = liveLinks(result.data);
  $('no-links').hidden = links.length > 0;
  $('links-wrap').hidden = links.length === 0;
  $('links-body').replaceChildren(...links.map(linkRow));
}

function linkRow(link) {
  const anyone = link.kind === 'anyone';
  const revoke = h(
    'button',
    { type: 'button', class: 'plain', 'aria-label': `Revoke ${linkLabel(link, link.role)}` },
    'Revoke',
  );
  revoke.addEventListener('click', () => askRevoke(link));
  const expiry = link.expires_at == null ? 'Never' : expiryText(link.expires_at).replace(/^expires /, '');
  return h(
    'tr',
    {},
    h('td', {}, anyone ? 'Anyone' : 'Signed-in'),
    h('td', {}, link.role?.name ?? ''),
    h('td', {}, expiry),
    h('td', {}, anyone ? 'Not counted' : String(link.members)),
    h('td', {}, revoke),
  );
}

$('link-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  note('', false);
  const anyone = $('link-kind').value === 'anyone';
  const { at, error: problem } = expiryFromDays($('link-days').value, {
    maxDays: sharing.anyone_link_max_days,
    required: anyone,
  });
  if (problem) return note(messageFor(problem), true);
  if (!$('link-role').value) return note('There is no role you can give by link here.', true);
  const body = { kind: anyone ? 'anyone' : 'signed_in', role: $('link-role').value };
  if (at) body.expires_at = at;
  const result = await api('POST', `/nodes/${seg(nodeId)}/links`, body);
  if (!result.ok) return note(messageFor(result.error), true);
  // The address is shown now and never again; it is not kept anywhere else.
  $('link-url').value = result.data.url;
  $('link-notice').textContent = ANYONE_NOTICE;
  $('link-new').hidden = false;
  $('link-url').focus();
  $('link-url').select();
  $('link-days').value = '';
  await refreshLinks();
  await refresh();
});

$('link-copy').addEventListener('click', async () => {
  const input = $('link-url');
  try {
    await navigator.clipboard.writeText(input.value);
    note('Copied.', false);
  } catch {
    input.focus();
    input.select();
    note('Press Ctrl+C to copy the address.', false);
  }
});

function askRevoke(link) {
  revoking = link;
  showMessage($('revoke-error'), '');
  $('revoke-text').textContent =
    link.kind === 'anyone'
      ? 'The link stops working at once for everyone who has it.'
      : 'The link stops working at once. People who joined through it lose the access it gave, unless they have it another way.';
  $('revoke-dialog').showModal();
}

$('revoke-cancel').addEventListener('click', () => $('revoke-dialog').close());
$('revoke-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  const link = revoking;
  const result = await api('DELETE', `/links/${seg(link.id)}`);
  if (!result.ok) return showMessage($('revoke-error'), messageFor(result.error));
  $('revoke-dialog').close();
  // The address just shown may be of the link revoked: it is of no use now.
  $('link-new').hidden = true;
  $('link-url').value = '';
  note('The link was revoked.', false);
  await refreshLinks();
  await refresh();
});

function fail(text) {
  showMessage(error, text);
  throw new Error(text);
}
