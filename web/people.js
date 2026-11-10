import { api, h, messageFor, requireMe, seg, showMessage } from './common.js';

const $ = (id) => document.getElementById(id);
const error = $('error');
const status = $('status');

let me;
try {
  me = await requireMe();
} catch (err) {
  status.textContent = err.message;
  throw err;
}
$('who').textContent = `Signed in as ${me.name}`;

if (!me.is_admin) {
  status.textContent = 'Only administrators can manage people.';
} else {
  $('content').hidden = false;
  $('add').addEventListener('submit', addPerson);
  await showUsers();
}

async function showUsers() {
  const result = await api('GET', '/admin/users');
  if (!result.ok) {
    showMessage(error, messageFor(result.error));
    return;
  }
  $('users').replaceChildren(
    ...result.data.map((user) => {
      const flags = [user.is_admin ? 'administrator' : '', user.disabled ? 'disabled' : '']
        .filter(Boolean)
        .join(', ');
      const action = user.disabled ? 'enable' : 'disable';
      const label = user.disabled ? 'Enable' : 'Disable';
      const button = h(
        'button',
        { type: 'button', class: 'plain', 'aria-label': `${label} ${user.name}` },
        label,
      );
      button.addEventListener('click', () => change(user, action));
      return h(
        'li',
        {},
        h('span', { class: 'grow' }, user.name, h('br'), h('small', {}, user.email)),
        h('small', {}, flags),
        button,
      );
    }),
  );
}

async function change(user, action) {
  showMessage(error, '');
  status.textContent = '';
  const result = await api('POST', `/admin/users/${seg(user.id)}/${action}`);
  if (!result.ok) showMessage(error, messageFor(result.error));
  else status.textContent = `${user.name} was ${action === 'enable' ? 'enabled' : 'disabled'}.`;
  await showUsers();
}

async function addPerson(event) {
  event.preventDefault();
  showMessage(error, '');
  status.textContent = '';
  const name = $('name').value.trim();
  const result = await api('POST', '/admin/users', {
    email: $('email').value.trim(),
    name,
    password: $('password').value,
    is_admin: $('admin').checked,
  });
  if (!result.ok) {
    showMessage(error, messageFor(result.error));
    return;
  }
  $('add').reset();
  status.textContent = `${name} was added.`;
  await showUsers();
}
