// Who can reach a site, library, folder or file, people and groups, and whether that comes from
// above. For those who manage access; the server decides, and refusals are shown as sentences.

import {
  api,
  can,
  groupLabel,
  h,
  inheritAgainMessage,
  messageFor,
  permsText,
  previewView,
  principalParts,
  queryId,
  requireMe,
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
if (!here) fail(MISSING);
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

$('inherit-cancel').addEventListener('click', () => $('inherit-dialog').close());
await refresh();

function note(text, isError) {
  error.classList.toggle('ok', !isError);
  showMessage(error, text);
}

/** Puts every role in `select` (the one with id `selected` chosen). */
function fillRoleSelect(select, selected, extra) {
  const list = extra && !roles.some((role) => role.id === extra.id) ? [...roles, extra] : roles;
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

function assignmentRow({ principal, role, perms }) {
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

function fail(text) {
  showMessage(error, text);
  throw new Error(text);
}
