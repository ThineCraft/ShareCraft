// People, for administrators: a directory of every account (avatar, name, email, Administrator
// and Disabled marks) with search as you type, a count at the top and an Actions menu per person
// (Disable or Enable, Copy email address). "Add a person" opens a dialog that sends the existing
// create route; refusals are shown as sentences in the dialog, success as a toast.

import { api, h, messageFor, organisationChrome, requireMe, seg, setupTopbar, showMessage } from './common.js';
import { avatarColour, initials } from './account.js';
import { setupMenu } from './menu.js';
import { listFilter, mountAdminShell, passwordCheck, peopleSummary } from './admin-shell.js';
import { showToast } from './toast.js';
import { settleAll } from './skeleton.js';

const $ = (id) => document.getElementById(id);
const error = $('error');
const status = $('status');
const dialog = $('add-dialog');

const passwordInput = $('password');
const toggle = $('password-toggle');

// The search field is in place from the start, so the list does not move when it comes.
const filter = listFilter({
  id: 'find-person',
  label: 'Find a person by name or email',
  nouns: ['person', 'people'],
  fields: ['name', 'email'],
  list: $('users'),
  empty: 'There is nobody here yet. Add the first person.',
});
$('users').before(filter.element);

let me;
try {
  me = await requireMe();
} catch (err) {
  settleAll();
  status.textContent = err.message;
  throw err;
}
setupTopbar(me, error);
mountAdminShell({ me, current: 'people' });
await organisationChrome();

if (!me.is_admin) {
  status.textContent = 'Only administrators can manage people.';
} else {
  $('content').hidden = false;
  $('add-button').hidden = false;
  $('add-button').addEventListener('click', openAdd);
  await showUsers();
}
settleAll(); // what could not be drawn is put back as it was (skeleton.js)

async function showUsers() {
  const result = await api('GET', '/admin/users');
  if (!result.ok) {
    showMessage(error, messageFor(result.error));
    return;
  }
  const people = [...result.data].sort((a, b) =>
    String(a.name || a.email).localeCompare(String(b.name || b.email), undefined, { sensitivity: 'base' }),
  );
  $('summary').textContent = peopleSummary(people);
  filter.set(people.map((person) => ({ row: person, element: personRow(person) })));
}

/** The round avatar of `person`, in the colour their id gives. */
function avatarOf(person) {
  const colour = avatarColour(person.id ?? person.email);
  const avatar = h('span', { class: 'avatar', 'aria-hidden': 'true' }, initials(person.name || person.email));
  avatar.style.setProperty('--avatar-light-bg', colour.light.bg);
  avatar.style.setProperty('--avatar-light-ink', colour.light.ink);
  avatar.style.setProperty('--avatar-dark-bg', colour.dark.bg);
  avatar.style.setProperty('--avatar-dark-ink', colour.dark.ink);
  return avatar;
}

function personRow(person) {
  const name = person.name || person.email;
  const menuId = `person-menu-${person.id}`;
  const button = h(
    'button',
    { type: 'button', class: 'plain menu-button dots', id: `person-actions-${person.id}`, 'aria-label': `Actions for ${name}` },
    h('span', { 'aria-hidden': 'true' }, '⋯'),
  );
  const toggle = h(
    'button',
    { type: 'button', role: 'menuitem', class: person.disabled ? null : 'danger' },
    person.disabled ? 'Enable' : 'Disable',
  );
  toggle.addEventListener('click', () => change(person, person.disabled ? 'enable' : 'disable'));
  const copy = h('button', { type: 'button', role: 'menuitem' }, 'Copy email address');
  copy.addEventListener('click', () => copyEmail(person));
  const menu = h(
    'ul',
    { id: menuId, class: 'menu end', 'aria-label': `Actions for ${name}` },
    h('li', { role: 'none' }, toggle),
    h('li', { role: 'none' }, copy),
  );
  setupMenu(button, menu);
  return h(
    'li',
    { class: person.disabled ? 'person disabled' : 'person' },
    avatarOf(person),
    h(
      'span',
      { class: 'grow person-text' },
      h('span', { class: 'person-name' }, name),
      h('small', { class: 'person-email' }, person.email),
    ),
    h(
      'span',
      { class: 'person-marks' },
      person.id === me.id ? h('small', { class: 'mark' }, 'You') : null,
      person.is_admin ? h('small', { class: 'mark own' }, 'Administrator') : null,
      person.disabled ? h('small', { class: 'mark warn' }, 'Disabled') : null,
    ),
    h('span', { class: 'menu-wrap' }, button, menu),
  );
}

/** Disables or enables `person`; the toast offers to turn it straight back. */
async function change(person, action, { undoing = false } = {}) {
  showMessage(error, '');
  const name = person.name || person.email;
  const result = await api('POST', `/admin/users/${seg(person.id)}/${action}`);
  if (!result.ok) {
    showToast(`Could not ${action} ${name}. ${messageFor(result.error)}`, { kind: 'error' });
    return;
  }
  await showUsers();
  const done = action === 'enable' ? `Enabled ${name}` : `Disabled ${name}. They are signed out everywhere.`;
  const back = action === 'enable' ? 'disable' : 'enable';
  showToast(done, undoing ? {} : { action: { label: 'Undo', run: () => change({ ...person, disabled: action === 'disable' }, back, { undoing: true }) } });
  // The row was drawn again: keep focus on this person's Actions button.
  document.getElementById(`person-actions-${person.id}`)?.focus();
}

async function copyEmail(person) {
  try {
    await navigator.clipboard.writeText(person.email);
    showToast(`Copied ${person.email}`);
  } catch {
    showToast('Could not copy. Select the address and copy it instead.', { kind: 'error' });
  }
}

// ---- Add a person ----

function showHint() {
  const check = passwordCheck(passwordInput.value);
  $('password-hint').textContent = check.text;
  $('password-hint').classList.toggle('ok', check.ok);
}

function openAdd() {
  $('add').reset();
  passwordInput.type = 'password';
  toggle.textContent = 'Show';
  toggle.setAttribute('aria-pressed', 'false');
  toggle.setAttribute('aria-label', 'Show password');
  showHint();
  showMessage($('add-error'), '');
  $('add-submit').disabled = false;
  dialog.showModal();
  $('email').focus();
}

toggle.addEventListener('click', () => {
  const showing = passwordInput.type === 'text';
  passwordInput.type = showing ? 'password' : 'text';
  toggle.textContent = showing ? 'Show' : 'Hide';
  toggle.setAttribute('aria-pressed', String(!showing));
  toggle.setAttribute('aria-label', showing ? 'Show password' : 'Hide password');
});
passwordInput.addEventListener('input', showHint);

// The buttons act on their own click: a dialog's close event may never come in a hidden tab.
$('add-cancel').addEventListener('click', () => dialog.close());

$('add').addEventListener('submit', async (event) => {
  event.preventDefault();
  showMessage($('add-error'), '');
  const email = $('email').value.trim();
  const name = $('name').value.trim();
  const check = passwordCheck(passwordInput.value);
  const problem = !email
    ? ['email', 'Give the person’s email address.']
    : !name
      ? ['name', 'Give the person’s name.']
      : !check.ok
        ? ['password', `The password needs ${check.text.charAt(0).toLowerCase()}${check.text.slice(1)}`]
        : null;
  if (problem) {
    showMessage($('add-error'), problem[1]);
    $(problem[0]).focus();
    return;
  }
  $('add-submit').disabled = true;
  const result = await api('POST', '/admin/users', {
    email,
    name,
    password: passwordInput.value,
    is_admin: $('admin').checked,
  });
  $('add-submit').disabled = false;
  if (!result.ok) {
    showMessage($('add-error'), messageFor(result.error));
    const field = { email_taken: 'email', invalid_email: 'email', invalid_name: 'name', weak_password: 'password' }[result.error];
    if (field) $(field).focus();
    return;
  }
  passwordInput.value = '';
  dialog.close();
  await showUsers();
  showToast(`Added ${name}`);
});
