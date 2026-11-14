// Roles, for administrators: the built-in ones are read-only; custom ones can be created,
// renamed, given other permissions and deleted. The server decides and refusals are sentences.

import {
  PERMS,
  api,
  confirmDialog,
  deleteRoleText,
  h,
  messageFor,
  permLabel,
  permsText,
  requireMe,
  seg,
  setupTopbar,
  showMessage,
} from './common.js';

const $ = (id) => document.getElementById(id);
const error = $('error');
const status = $('status');

const me = await requireMe();
setupTopbar(me, error);

if (!me.is_admin) {
  status.textContent = 'Only administrators can manage roles.';
} else {
  $('content').hidden = false;
  $('new-perms').append(...permBoxes('new', ['view']));
  $('new-role').addEventListener('submit', create);
  await showRoles();
}

function note(text, isError) {
  error.classList.toggle('ok', !isError);
  showMessage(error, text);
}

/** The six check boxes for `held` permissions; `view` is always on and cannot be cleared. */
function permBoxes(prefix, held) {
  return PERMS.map((perm) => {
    const id = `${prefix}-${perm}`;
    const box = h('input', { type: 'checkbox', id, value: perm });
    box.checked = perm === 'view' || held.includes(perm);
    box.disabled = perm === 'view';
    return h('label', { class: 'check', for: id }, box, permLabel(perm));
  });
}

/** The permissions ticked in `scope` (an element holding permBoxes), `view` always among them. */
function ticked(scope) {
  const names = [...scope.querySelectorAll('input[type=checkbox]')]
    .filter((box) => box.checked)
    .map((box) => box.value);
  return names.includes('view') ? names : ['view', ...names];
}

async function showRoles() {
  const result = await api('GET', '/roles');
  if (!result.ok) return note(messageFor(result.error), true);
  $('roles').replaceChildren(...result.data.map(roleRow));
}

function roleRow(role) {
  if (role.builtin) {
    return h(
      'li',
      {},
      h('span', { class: 'grow' }, role.name, h('br'), h('small', {}, `Can ${permsText(role.perms)}`)),
      h('small', { class: 'mark' }, 'Built in'),
    );
  }
  const nameId = `name-${role.id}`;
  const name = h('input', { type: 'text', id: nameId, maxlength: 60, autocomplete: 'off', required: true, value: role.name });
  const boxes = h('fieldset', { class: 'perms' }, h('legend', {}, `Permissions of ${role.name}`), ...permBoxes(role.id, role.perms));
  const save = h('button', { type: 'submit' }, 'Save');
  const remove = h('button', { type: 'button', class: 'plain', 'aria-label': `Delete ${role.name}` }, 'Delete');
  const form = h(
    'form',
    { class: 'role-form' },
    h('div', { class: 'field' }, h('label', { for: nameId }, 'Name'), name),
    boxes,
    h('span', { class: 'actions' }, save, remove),
  );
  form.addEventListener('submit', async (event) => {
    event.preventDefault();
    note('', false);
    const result = await api('PATCH', `/roles/${seg(role.id)}`, { name: name.value.trim(), perms: ticked(boxes) });
    note(result.ok ? `${result.data.name} was saved.` : messageFor(result.error), !result.ok);
    await showRoles();
  });
  remove.addEventListener('click', async () => {
    note('', false);
    if (!(await confirmDialog(deleteRoleText(role), 'Delete role'))) return;
    const result = await api('DELETE', `/roles/${seg(role.id)}`);
    note(result.ok ? `${role.name} was deleted.` : messageFor(result.error), !result.ok);
    await showRoles();
  });
  return h('li', { class: 'role' }, form);
}

async function create(event) {
  event.preventDefault();
  note('', false);
  const result = await api('POST', '/roles', {
    name: $('role-name').value.trim(),
    perms: ticked($('new-perms')),
  });
  if (!result.ok) return note(messageFor(result.error), true);
  $('role-name').value = '';
  note(`${result.data.name} was created.`, false);
  await showRoles();
}
