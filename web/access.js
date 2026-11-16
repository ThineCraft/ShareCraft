// The Share screen: who can reach a site, library, folder or file, said in one summary at the
// top; one "Add people or groups" box; the entries as rows with a role menu; and the links as
// cards. For those who manage access; the server decides, and refusals are shown as sentences.

import {
  ANYONE_NOTICE,
  ageText,
  api,
  can,
  expiryFromDays,
  expiryText,
  formatDate,
  h,
  inheritAgainMessage,
  linkLabel,
  linkRevokeHint,
  linkRoleChoices,
  liveLinks,
  messageFor,
  offerRequest,
  previewView,
  principalParts,
  queryId,
  requireMe,
  rolesWithin,
  seg,
  setupTopbar,
  showCrumbs,
  showMessage,
  siteChrome,
} from './common.js';
import { avatarColour, initials } from './account.js';
import { setupMenu } from './menu.js';
import { showToast } from './toast.js';
import { accessSummary, comboboxFilter, looksLikeEmail, permsSentence, sortEntries } from './share.js';
import { settle, settleAll } from './skeleton.js';

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
await siteChrome(here.path[0].id, { currentId: nodeId });
const self = here.path[here.path.length - 1];
$('title').textContent = `Share ${self.name}`;
document.title = `Share ${self.name} — ShareCraft`;
if (!can(here.access, 'manage_access')) fail('Only people who manage access here can see or change who has access.');
$('explorer-link').href = `/explorer.html?root=${seg(nodeId)}`;

let state = null; // { inherits, source, assignments } as the server last said
let roles = []; // every role, built-in first, as GET /api/roles lists them
let groups = []; // the groups this person may see, as GET /api/groups lists them
let people = []; // accounts to suggest: GET /api/admin/users, which only administrators may call
let links = []; // the live links of this item, as GET /api/nodes/{id}/links lists them
let revoking = null;
let pending = []; // chosen in the Add box, not yet shared: { kind, id, name, email?, detail }
let menuCount = 0; // numbers the role menus, for their ids
// Addresses of links made while this page is open. The server shows an address once, when the
// link is made, and keeps no copy; it lives only in this page until it is closed or reloaded.
const madeNow = new Map();

const [rolesResult, groupsResult, sharingResult] = await Promise.all([
  api('GET', '/roles'),
  api('GET', '/groups'),
  api('GET', '/settings/sharing'),
]);
if (rolesResult.ok) roles = rolesResult.data;
if (groupsResult.ok) groups = groupsResult.data;
if (me.is_admin) {
  const peopleResult = await api('GET', '/admin/users');
  if (peopleResult.ok) people = peopleResult.data.filter((person) => !person.disabled);
}
fillRoleSelect($('share-role'), 'editor');

// What links may be made here: anyone links only while an administrator allows them.
let sharing = { anyone_links: false, anyone_link_max_days: 30 };
if (sharingResult.ok) sharing = sharingResult.data;
if (!sharing.anyone_links) $('anyone-option').remove();
else $('anyone-option').hidden = false;

/** A refusal stays above the page; what was done is said in a toast. */
function note(text, isError) {
  error.classList.remove('ok');
  if (text && !isError) {
    showMessage(error, '');
    showToast(text);
    return;
  }
  showMessage(error, text);
}

/** Says `text` to screen readers without showing it (the status line). */
function say(text) {
  $('status').textContent = '';
  $('status').textContent = text;
}

/** Only the roles this person may give; `extra` (an entry's current role) is kept in its list. */
function roleChoices(extra) {
  const allowed = rolesWithin(roles, here.access);
  return extra && !allowed.some((role) => role.id === extra.id) ? [...allowed, extra] : allowed;
}

/** Puts every role this person may give in `select` (the one with id `selected` chosen). */
function fillRoleSelect(select, selected) {
  select.replaceChildren(
    ...roleChoices().map((role) => {
      const option = h('option', { value: role.id }, role.name);
      option.selected = role.id === selected;
      return option;
    }),
  );
}

/** A round avatar with initials, coloured from `key`; a group's is a rounded square. */
function avatar(name, key, { group = false, small = false } = {}) {
  const colour = avatarColour(key);
  const element = h('span', { class: `avatar${small ? ' small' : ''}${group ? ' group' : ''}`, 'aria-hidden': 'true' }, initials(name));
  element.style.setProperty('--avatar-light-bg', colour.light.bg);
  element.style.setProperty('--avatar-light-ink', colour.light.ink);
  element.style.setProperty('--avatar-dark-bg', colour.dark.bg);
  element.style.setProperty('--avatar-dark-ink', colour.dark.ink);
  return element;
}

// ---- who has access ----

async function refresh() {
  const result = await api('GET', `/nodes/${seg(nodeId)}/access`);
  if (!result.ok) {
    $('content').hidden = true;
    $('source-row').hidden = true;
    note(result.status === 404 ? MISSING : messageFor(result.error), true);
    if (result.status === 404) offerRequest(error, nodeId);
    return;
  }
  state = result.data;
  show();
}

/** The access entries, each group carrying its member count when the person may see the group. */
function entriesWithCounts() {
  const counts = new Map(groups.map((group) => [group.id, group.member_count]));
  return state.assignments.map((entry) =>
    entry.principal.kind === 'group' && counts.has(entry.principal.id)
      ? { ...entry, principal: { ...entry.principal, member_count: counts.get(entry.principal.id) } }
      : entry,
  );
}

function showSummary() {
  if (state) $('summary').textContent = accessSummary(entriesWithCounts(), links);
}

/** The breadcrumb, with the place access comes from marked. */
function showTrail() {
  const sourceId = state.source?.id;
  for (const item of $('crumbs').querySelectorAll('li')) item.classList.remove('source');
  const steps = [...$('crumbs').querySelectorAll('li')];
  const at = here.path.findIndex((step) => step.id === sourceId);
  const step = steps[at];
  if (!step) return;
  step.classList.add('source');
  step.append(h('span', { class: 'sr' }, state.inherits ? ' (access comes from here)' : ' (access is set here)'));
}

function show() {
  $('content').hidden = false;
  $('source-row').hidden = false;
  const isSite = self.kind === 'site';
  const source = $('source');
  if (state.inherits) {
    source.replaceChildren(
      h('span', { class: 'source-badge' }, 'Inherits from ', h('a', { href: `/access.html?id=${seg(state.source.id)}` }, state.source.name)),
      ' Changes there reach this item.',
    );
  } else if (isSite) {
    source.replaceChildren(h('span', { class: 'source-badge own' }, 'Its own list'), ' A site has its own people and groups. It cannot inherit.');
  } else {
    source.replaceChildren(h('span', { class: 'source-badge own' }, 'Its own list'), ' Changes above do not reach this item.');
  }
  $('inherit-note').textContent = state.inherits
    ? 'Stopping inheritance copies the entries below onto this item, so you can change them here without changing the place above.'
    : '';
  const toggle = $('inherit-toggle');
  toggle.hidden = isSite;
  toggle.textContent = state.inherits ? 'Stop inheriting' : 'Inherit again';
  $('share-hint').hidden = !state.inherits;
  showTrail();
  showSummary();
  const entries = sortEntries(entriesWithCounts());
  $('no-assignments').hidden = entries.length > 0;
  $('assignments').replaceChildren(...entries.map(assignmentRow));
  refreshOptions();
  putFocusBack();
}

// After a change redraws the rows, focus goes back to the row changed, or the one now in the place
// of a row removed (the Add box when none is left), so keyboard users are not sent to the top.
let refocus = null; // { key } or { index }

function putFocusBack() {
  if (!refocus) return;
  const wanted = refocus;
  refocus = null;
  const buttons = [...$('assignments').querySelectorAll('.role-button')];
  const byKey = wanted.key ? $('assignments').querySelector(`[data-key="${CSS.escape(wanted.key)}"] .role-button`) : null;
  const target = byKey ?? (wanted.index != null ? buttons[Math.min(wanted.index, buttons.length - 1)] : null);
  (target ?? find).focus();
}

// Where a link is revoked: here, or (on a break below holding a copy) on its own item's page.
function revokeHint(principal) {
  const hint = linkRevokeHint(principal, nodeId);
  const on = hint.on;
  const text = hint.text.replace('under Share by link', 'under Links'); // the section's name here
  if (!on) return h('small', { class: 'meta' }, text);
  return h('small', { class: 'meta' }, `${text} on `, h('a', { href: on.href }, on.name));
}

/** `group · site: Home`, `person · ann@example.com`. */
function detailOf(principal) {
  if (principal.hidden) return 'group';
  const { note: sub } = principalParts(principal);
  const kind = principal.kind === 'group' ? 'group' : 'person';
  return sub ? `${kind} · ${sub}` : kind;
}


function assignmentRow({ principal, role, perms }) {
  if (principal.kind === 'link') {
    // A link is changed under Links, never as a person or group.
    const name = principalParts(principal, role).name;
    return h(
      'li',
      { class: 'share-entry' },
      h('span', { class: 'avatar link-mark', 'aria-hidden': 'true' }),
      h('span', { class: 'grow' }, h('span', { class: 'entry-name' }, name), h('small', {}, `${permsSentence(perms)}.`)),
      revokeHint(principal),
    );
  }
  const { name, hidden } = principalParts(principal);
  const isUser = principal.kind === 'user';
  const target = isUser ? { email: principal.email } : { group: principal.id };
  const changeable = !(hidden || (isUser && !principal.email));
  const pic = hidden
    ? h('span', { class: 'avatar group hidden-group', 'aria-hidden': 'true' }, '?')
    : avatar(name, principal.id, { group: !isUser });

  menuCount += 1;
  const menuId = `role-menu-${menuCount}`;
  const button = h(
    'button',
    { type: 'button', class: 'plain menu-button role-button', id: `role-button-${menuCount}`, 'aria-label': `${role.name}: change the role of ${name}, or remove` },
    role.name,
    h('span', { class: 'caret', 'aria-hidden': 'true' }, '▾'),
  );
  const items = [];
  if (changeable) {
    items.push(h('li', { role: 'none', class: 'menu-label', 'aria-hidden': 'true' }, 'Role'));
    const radios = roleChoices(role).map((choice) => {
      const radio = h(
        'button',
        { type: 'button', role: 'menuitemradio', 'aria-checked': String(choice.id === role.id) },
        h('span', { class: 'radio-dot', 'aria-hidden': 'true' }),
        choice.name,
      );
      radio.addEventListener('click', () => {
        controls.close(true);
        if (choice.id !== role.id) setRole(target, choice, name, `${principal.kind}:${principal.id}`);
      });
      return h('li', { role: 'none' }, radio);
    });
    items.push(h('li', { role: 'none' }, h('ul', { role: 'group', class: 'menu-group', 'aria-label': 'Role' }, ...radios)));
    items.push(h('li', { role: 'separator', class: 'menu-separator' }));
  }
  const remove = h('button', { type: 'button', role: 'menuitem', class: 'danger' }, `Remove ${hidden ? 'this group' : 'access'}`);
  remove.addEventListener('click', () => removeEntry(principal, name));
  items.push(h('li', { role: 'none' }, remove));
  const menu = h('ul', { id: menuId, class: 'menu end', 'aria-label': `Role of ${name}` }, ...items);
  const controls = setupMenu(button, menu);

  return h(
    'li',
    { class: 'share-entry', 'data-key': `${principal.kind}:${principal.id}` },
    pic,
    h(
      'span',
      { class: 'grow' },
      h('span', { class: 'entry-name' }, name),
      h('small', {}, detailOf(principal)),
      h('small', { class: 'entry-perms' }, `${permsSentence(perms)}.`),
    ),
    h('span', { class: 'menu-wrap' }, button, menu),
  );
}

async function setRole(target, role, name, key) {
  note('', false);
  refocus = { key };
  const result = await api('PUT', `/nodes/${seg(nodeId)}/access`, { ...target, role: role.id });
  note(result.ok ? `${name} is now ${role.name}.` : messageFor(result.error), !result.ok);
  await refresh();
}

async function removeEntry(principal, name) {
  note('', false);
  const rows = [...$('assignments').querySelectorAll('.role-button')];
  refocus = { index: Math.max(0, rows.indexOf(document.activeElement)) };
  const result = await api('DELETE', `/nodes/${seg(nodeId)}/access/${seg(principal.kind)}/${seg(principal.id)}`);
  note(result.ok ? `${name} was removed.` : messageFor(result.error), !result.ok);
  await refresh();
}

// ---- the Add people or groups box ----

const find = $('share-find');
const listbox = $('share-options');
let options = []; // the suggestions shown
let active = -1; // the suggestion marked, or -1

/** Every group and person that could be added: not listed already, not chosen already. */
function candidates() {
  const listed = new Set((state?.assignments ?? []).map((entry) => `${entry.principal.kind}:${entry.principal.id}`));
  const listedEmails = new Set(
    (state?.assignments ?? []).map((entry) => String(entry.principal.email ?? '').toLowerCase()).filter(Boolean),
  );
  const chosen = new Set(pending.map((item) => `${item.kind}:${item.id}`));
  const out = [];
  for (const group of groups) {
    const key = `group:${group.id}`;
    if (listed.has(key) || chosen.has(key)) continue;
    const where = group.site ? `site: ${group.site.name}` : 'organisation group';
    out.push({ kind: 'group', id: group.id, name: group.name, detail: `group · ${where}`, count: group.member_count });
  }
  for (const person of people) {
    const key = `user:${person.id}`;
    if (listed.has(key) || chosen.has(key) || listedEmails.has(person.email.toLowerCase())) continue;
    out.push({ kind: 'user', id: person.id, name: person.name || person.email, email: person.email, detail: `person · ${person.email}` });
  }
  return out;
}

function optionRow(option, index) {
  const isGroup = option.kind === 'group';
  const pic =
    option.kind === 'email'
      ? h('span', { class: 'avatar small email-mark', 'aria-hidden': 'true' }, '@')
      : avatar(option.name, option.id, { group: isGroup, small: true });
  const count = isGroup && Number.isFinite(option.count) ? ` · ${option.count} ${option.count === 1 ? 'person' : 'people'}` : '';
  const row = h(
    'li',
    { role: 'option', id: `share-option-${index}`, 'aria-selected': String(index === active), class: 'combo-option' },
    pic,
    h('span', { class: 'combo-text' }, h('span', { class: 'combo-name' }, option.name), h('small', {}, `${option.detail}${count}`)),
  );
  // Choosing with the pointer keeps focus in the box.
  row.addEventListener('mousedown', (event) => event.preventDefault());
  row.addEventListener('click', () => choose(option));
  return row;
}

function openList(show) {
  listbox.hidden = !show;
  find.setAttribute('aria-expanded', String(show));
  if (!show) {
    active = -1;
    find.removeAttribute('aria-activedescendant');
  }
}

/** Fills the suggestions for what is typed now; `open` shows them even for an empty box. */
function refreshOptions(open = !listbox.hidden) {
  options = comboboxFilter(find.value, candidates());
  if (active >= options.length) active = options.length - 1;
  listbox.replaceChildren(
    ...(options.length ? options.map(optionRow) : [h('li', { class: 'combo-empty', role: 'presentation' }, find.value.trim() ? 'No group or person matches. Type a whole email address to add someone by email.' : 'No groups or people left to add.')]),
  );
  openList(open && document.activeElement === find);
  markActive();
}

function markActive() {
  for (const [index, row] of [...listbox.querySelectorAll('[role=option]')].entries()) {
    row.setAttribute('aria-selected', String(index === active));
    if (index === active) row.scrollIntoView?.({ block: 'nearest' });
  }
  if (active >= 0 && !listbox.hidden) find.setAttribute('aria-activedescendant', `share-option-${active}`);
  else find.removeAttribute('aria-activedescendant');
}

function choose(option) {
  pending.push(option);
  find.value = '';
  active = -1;
  showPending();
  refreshOptions(false);
  say(`${option.name} added to the list. ${pending.length} to share.`);
  find.focus();
}

function showPending() {
  const list = $('pending');
  list.hidden = pending.length === 0;
  list.replaceChildren(
    ...pending.map((item) => {
      const remove = h('button', { type: 'button', class: 'chip-remove', 'aria-label': `Take ${item.name} off the list` }, '×');
      remove.addEventListener('click', () => {
        pending = pending.filter((other) => other !== item);
        showPending();
        say(`${item.name} taken off the list.`);
        find.focus();
      });
      const pic = item.kind === 'email'
        ? h('span', { class: 'avatar small email-mark', 'aria-hidden': 'true' }, '@')
        : avatar(item.name, item.id, { group: item.kind === 'group', small: true });
      return h('li', { class: 'person-chip pending-chip' }, pic, h('span', {}, item.name), remove);
    }),
  );
}

find.addEventListener('input', () => {
  active = -1;
  refreshOptions(true);
});
find.addEventListener('focus', () => {
  if (find.value.trim()) refreshOptions(true);
});
find.addEventListener('blur', () => openList(false));
find.addEventListener('click', () => refreshOptions(true));
find.addEventListener('keydown', (event) => {
  const open = !listbox.hidden;
  switch (event.key) {
    case 'ArrowDown':
    case 'ArrowUp': {
      event.preventDefault();
      if (!open) {
        refreshOptions(true);
        active = options.length ? (event.key === 'ArrowDown' ? 0 : options.length - 1) : -1;
      } else if (options.length) {
        const step = event.key === 'ArrowDown' ? 1 : -1;
        active = active === -1 ? (step === 1 ? 0 : options.length - 1) : (active + step + options.length) % options.length;
      }
      markActive();
      break;
    }
    case 'Enter':
      if (open && active >= 0 && options[active]) {
        event.preventDefault();
        choose(options[active]);
      } else if (find.value.trim()) {
        // A typed address, or a single suggestion left, is taken; anything else stays to be fixed.
        event.preventDefault();
        const typed = find.value.trim();
        const known = options.find((o) => o.email?.toLowerCase() === typed.toLowerCase());
        if (looksLikeEmail(typed)) choose(known ?? { kind: 'email', id: `email:${typed.toLowerCase()}`, name: typed, email: typed, detail: 'Add by email address' });
        else if (options.length === 1) choose(options[0]);
        else say(options.length ? 'Choose one of the suggestions with the arrow keys.' : 'Nothing matches. Type a whole email address.');
      }
      break;
    case 'Escape':
      if (open) {
        event.preventDefault();
        openList(false);
      } else if (find.value) {
        event.preventDefault();
        find.value = '';
      }
      break;
    case 'Backspace':
      if (find.value === '' && pending.length) {
        const item = pending.pop();
        showPending();
        say(`${item.name} taken off the list.`);
      }
      break;
    default:
      break;
  }
});

$('share-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  note('', false);
  const typed = find.value.trim();
  if (typed) {
    if (!looksLikeEmail(typed)) {
      return note('Choose a person or group from the suggestions, or type a whole email address.', true);
    }
    pending.push({ kind: 'email', id: `email:${typed.toLowerCase()}`, name: typed, email: typed, detail: '' });
    find.value = '';
  }
  if (pending.length === 0) return note('Choose a person or group first, or type an email address.', true);
  const role = $('share-role').value;
  const button = $('share-submit');
  button.disabled = true;
  const done = [];
  const problems = [];
  const left = [];
  for (const item of pending) {
    const body = item.kind === 'group' ? { group: item.id, role } : { email: item.email, role };
    const result = await api('PUT', `/nodes/${seg(nodeId)}/access`, body);
    if (result.ok) done.push(item.name);
    else {
      left.push(item);
      problems.push(pending.length === 1 ? messageFor(result.error) : `${item.name}: ${messageFor(result.error)}`);
    }
  }
  button.disabled = false;
  pending = left;
  showPending();
  const roleName = roles.find((r) => r.id === role)?.name ?? role;
  const said = done.length ? `Shared with ${done.join(', ')} as ${roleName}.` : '';
  if (problems.length) note([said, ...problems].filter(Boolean).join(' '), true);
  else note(said, false);
  await refresh();
});

// ---- inheritance ----

$('inherit-cancel').addEventListener('click', () => $('inherit-dialog').close());

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

// ---- links ----

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

$('link-make').addEventListener('click', () => {
  $('link-fields').hidden = false;
  $('link-new').hidden = true;
  $('link-url').value = '';
  $('link-days').value = '';
  $('link-submit').textContent = 'Make link';
  $('link-title').textContent = 'Make a link';
  $('link-cancel').hidden = false;
  showMessage($('link-error'), '');
  fillLinkForm();
  $('link-dialog').showModal();
  $('link-kind').focus();
});
$('link-cancel').addEventListener('click', () => $('link-dialog').close());

$('link-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  if ($('link-fields').hidden) return $('link-dialog').close(); // Done, after the address was shown
  showMessage($('link-error'), '');
  const anyone = $('link-kind').value === 'anyone';
  const { at, error: problem } = expiryFromDays($('link-days').value, {
    maxDays: sharing.anyone_link_max_days,
    required: anyone,
  });
  if (problem) return showMessage($('link-error'), messageFor(problem));
  if (!$('link-role').value) return showMessage($('link-error'), 'There is no role you can give by link here.');
  const body = { kind: anyone ? 'anyone' : 'signed_in', role: $('link-role').value };
  if (at) body.expires_at = at;
  const result = await api('POST', `/nodes/${seg(nodeId)}/links`, body);
  if (!result.ok) return showMessage($('link-error'), messageFor(result.error));
  // The address is shown now; the server keeps no copy, and this page keeps it only while open.
  madeNow.set(result.data.id, result.data.url);
  $('link-fields').hidden = true;
  $('link-url').value = result.data.url;
  $('link-notice').textContent = ANYONE_NOTICE;
  $('link-new').hidden = false;
  $('link-submit').textContent = 'Done';
  $('link-title').textContent = 'Your new link';
  $('link-cancel').hidden = true;
  $('link-url').focus();
  $('link-url').select();
  note('The link was made.', false);
  await refreshLinks();
  await refresh();
});

async function copyAddress(url, input) {
  try {
    await navigator.clipboard.writeText(url);
    note('Copied.', false);
  } catch {
    if (input) {
      input.focus();
      input.select();
      note('Press Ctrl+C to copy the address.', false);
    } else {
      note(`Copying did not work. The address is ${url}`, true);
    }
  }
}

$('link-copy').addEventListener('click', () => copyAddress($('link-url').value, $('link-url')));

async function refreshLinks() {
  const result = await api('GET', `/nodes/${seg(nodeId)}/links`);
  if (!result.ok) return note(messageFor(result.error), true);
  links = liveLinks(result.data);
  $('no-links').hidden = links.length > 0;
  $('links-list').replaceChildren(...links.map(linkCard));
  showSummary();
}

function fact(term, value) {
  return h('div', { class: 'fact' }, h('dt', {}, term), h('dd', {}, value));
}

function linkCard(link) {
  const anyone = link.kind === 'anyone';
  const label = linkLabel(link, link.role);
  const revoke = h('button', { type: 'button', class: 'plain', 'aria-label': `Revoke ${label}` }, 'Revoke');
  revoke.addEventListener('click', () => askRevoke(link));
  const url = madeNow.get(link.id);
  let address;
  if (url) {
    const copy = h('button', { type: 'button', 'aria-label': `Copy the address of ${label}` }, 'Copy');
    copy.addEventListener('click', () => copyAddress(url));
    address = copy;
  } else {
    address = h('span', { class: 'meta' }, 'Address shown once when made');
  }
  const expiry = link.expires_at == null ? 'Never' : expiryText(link.expires_at).replace(/^expires /, '');
  const made = [link.created_by?.name, link.created_at ? ageText(link.created_at) : null].filter(Boolean).join(', ');
  return h(
    'li',
    { class: 'link-card' },
    h(
      'div',
      { class: 'link-card-head' },
      h('span', { class: `link-kind${anyone ? ' anyone' : ''}`, 'aria-hidden': 'true' }),
      h('h3', {}, anyone ? 'Anyone with the link' : 'People signed in here'),
    ),
    h(
      'dl',
      { class: 'link-facts' },
      fact('Role', link.role?.name ?? ''),
      fact('Expires', expiry),
      fact('Used by', anyone ? 'Not counted' : `${link.members} ${link.members === 1 ? 'person' : 'people'}`),
      made ? fact('Made by', h('span', { title: link.created_at ? formatDate(link.created_at) : '' }, made)) : null,
    ),
    h('div', { class: 'link-card-actions' }, address, revoke),
  );
}

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
  // The address made for it is of no use now.
  madeNow.delete(link.id);
  note('The link was revoked.', false);
  await refreshLinks();
  await refresh();
});

// Everything above is wired; now the page is drawn.
await refresh();
await refreshLinks();
// The head is drawn: its height is remembered; what could not be drawn is put back (skeleton.js).
settle(document.querySelector('.share-head'), { drawn: Boolean(state) });
settleAll();

function fail(text) {
  settleAll();
  showMessage(error, text);
  throw new Error(text);
}
