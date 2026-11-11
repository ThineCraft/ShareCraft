// Who can reach a site, library, folder or file, and whether that comes from above. Owners
// only; the server decides, and refusals are shown as sentences.

import {
  ROLES,
  api,
  h,
  messageFor,
  queryId,
  requireMe,
  roleName,
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
const above = here.path.length > 1 ? here.path[here.path.length - 2] : null;
$('title').textContent = `Access to ${self.name}`;
document.title = `Access to ${self.name} — ShareCraft`;
if (here.role !== 'owner') fail('Only owners can see or change who has access.');

let state = null; // { inherits, source, grants } as the server last said

$('inherit-cancel').addEventListener('click', () => $('inherit-dialog').close());
await refresh();

function note(text, isError) {
  error.classList.toggle('ok', !isError);
  showMessage(error, text);
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
      '. The people below are listed there.',
    );
  } else if (isSite) {
    source.textContent = 'A site has its own list of people. It cannot inherit.';
  } else {
    source.textContent = 'This item has its own list of people. Changes above do not reach it.';
  }
  $('inherit-note').textContent = state.inherits
    ? 'Stopping inheritance copies the people below onto this item, so you can change them here without changing the place above.'
    : '';
  const toggle = $('inherit-toggle');
  toggle.hidden = isSite;
  toggle.textContent = state.inherits ? 'Stop inheriting' : 'Inherit again';
  $('share-hint').hidden = !state.inherits;
  $('grants').replaceChildren(...state.grants.map(personRow));
}

function personRow({ user, role }) {
  const select = h('select', { 'aria-label': `Role of ${user.name}` });
  for (const value of ROLES) {
    const option = h('option', { value }, roleName(value));
    option.selected = value === role;
    select.append(option);
  }
  select.addEventListener('change', () => setRole(user.email, select.value));
  const remove = h(
    'button',
    { type: 'button', class: 'plain', 'aria-label': `Remove ${user.name}` },
    'Remove',
  );
  remove.addEventListener('click', () => removePerson(user));
  return h(
    'li',
    {},
    h('span', { class: 'grow' }, user.name, h('br'), h('small', {}, user.email)),
    select,
    remove,
  );
}

async function setRole(email, role) {
  note('', false);
  const result = await api('PUT', `/nodes/${seg(nodeId)}/access`, { email, role });
  note(result.ok ? 'Saved.' : messageFor(result.error), !result.ok);
  await refresh();
}

async function removePerson(user) {
  note('', false);
  const result = await api('DELETE', `/nodes/${seg(nodeId)}/access/${seg(user.id)}`);
  note(result.ok ? `${user.name} was removed.` : messageFor(result.error), !result.ok);
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

$('inherit-toggle').addEventListener('click', async () => {
  note('', false);
  if (state.inherits) {
    const result = await api('POST', `/nodes/${seg(nodeId)}/inheritance`, { inherit: false });
    note(result.ok ? 'This item now has its own list of people.' : messageFor(result.error), !result.ok);
    await refresh();
    return;
  }
  const count = state.grants.length;
  const people = `${count} ${count === 1 ? 'person has' : 'people have'}`;
  const from = above ? `“${above.name}”` : 'the place above';
  $('inherit-text').textContent =
    `${people} an entry of their own on “${self.name}”. Inheriting again removes ` +
    `${count === 1 ? 'that entry' : 'those entries'}, and access comes from ${from} instead.`;
  showMessage($('inherit-error'), '');
  $('inherit-dialog').showModal();
});

$('inherit-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  const result = await api('POST', `/nodes/${seg(nodeId)}/inheritance`, { inherit: true });
  if (!result.ok) return showMessage($('inherit-error'), messageFor(result.error));
  $('inherit-dialog').close();
  note('This item inherits its access again.', false);
  await refresh();
});

function fail(text) {
  showMessage(error, text);
  throw new Error(text);
}
