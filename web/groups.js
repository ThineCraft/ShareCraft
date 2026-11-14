// Groups and their members: the three groups of one site (`?site=`), one group (`?id=`) or every
// group the person may see. Only those who manage a group see or change its members; the server
// decides, and refusals are shown as sentences.

import {
  api,
  confirmDialog,
  groupLabel,
  h,
  messageFor,
  pageFor,
  queryId,
  removeMemberText,
  requireMe,
  seg,
  setupTopbar,
  showMessage,
} from './common.js';

const $ = (id) => document.getElementById(id);
const error = $('error');

const me = await requireMe();
setupTopbar(me, error);
const siteId = queryId(location.search, 'site');
const groupId = queryId(location.search, 'id');
const MISSING = 'These groups do not exist, or you cannot see them.';

$('new-group').hidden = !(me.is_admin && !siteId && !groupId);
$('new-group').addEventListener('submit', async (event) => {
  event.preventDefault();
  note('', false);
  const result = await api('POST', '/groups', { name: $('group-name').value.trim() });
  if (result.ok) $('group-name').value = '';
  note(result.ok ? 'The group was created.' : messageFor(result.error), !result.ok);
  await load();
});

await load();

function note(text, isError) {
  error.classList.toggle('ok', !isError);
  showMessage(error, text);
}

/** Loads and shows whatever the address asks for. */
async function load() {
  let list;
  if (siteId) {
    const result = await api('GET', `/sites/${seg(siteId)}/groups`);
    if (!result.ok) return failed(result);
    list = result.data;
    const site = list[0]?.site;
    $('title').textContent = site ? `Groups of ${site.name}` : 'Groups';
    document.title = `${$('title').textContent} — ShareCraft`;
    $('site-link').hidden = false;
    $('site-link').href = pageFor('site', siteId);
    $('site-link').textContent = site?.name ?? 'Site';
  } else if (groupId) {
    const result = await api('GET', `/groups/${seg(groupId)}`);
    if (!result.ok) return failed(result);
    list = [result.data];
    $('title').textContent = groupLabel(result.data);
    document.title = `${$('title').textContent} — ShareCraft`;
  } else {
    const result = await api('GET', '/groups');
    if (!result.ok) return failed(result);
    list = result.data;
  }
  $('empty').hidden = list.length > 0;
  const sections = [];
  for (const group of list) sections.push(await section(group));
  $('groups').replaceChildren(...sections);
}

function failed(result) {
  $('empty').hidden = true;
  $('groups').replaceChildren();
  note(result.status === 404 ? MISSING : messageFor(result.error), true);
}

/** One group: its name, its members (for those who manage it), adding and removing them. */
async function section(group) {
  let members = group.members;
  if (members === undefined && group.can_manage) {
    const detail = await api('GET', `/groups/${seg(group.id)}`);
    members = detail.ok ? detail.data.members : [];
  }
  const count = `${group.member_count} ${group.member_count === 1 ? 'member' : 'members'}`;
  const heading = h(
    'h2',
    {},
    group.site || groupId ? group.name : h('a', { href: `/groups.html?id=${seg(group.id)}` }, group.name),
  );
  heading.append(' ', h('small', { class: 'meta' }, group.site ? `site: ${group.site.name}` : 'organisation group'));
  const parts = [heading, h('p', { class: 'meta' }, count)];
  if (!group.can_manage) {
    parts.push(h('p', { class: 'meta' }, 'Only people who manage this group can see or change its members.'));
  } else {
    parts.push(
      members.length
        ? h('ul', { class: 'list' }, ...members.map((member) => memberRow(group, member)))
        : h('p', { class: 'meta' }, 'There are no members yet.'),
      addForm(group),
    );
  }
  if (me.is_admin && !group.site) {
    const remove = h('button', { type: 'button', class: 'plain' }, `Delete ${group.name}`);
    remove.addEventListener('click', () => deleteGroup(group));
    parts.push(h('p', {}, remove));
  }
  return h('section', { 'aria-label': groupLabel(group) }, ...parts);
}

function memberRow(group, member) {
  const remove = h(
    'button',
    { type: 'button', class: 'plain', 'aria-label': `Remove ${member.name} from ${group.name}` },
    'Remove',
  );
  remove.addEventListener('click', async () => {
    note('', false);
    if (!(await confirmDialog(removeMemberText(member, group), 'Remove'))) return;
    const result = await api('DELETE', `/groups/${seg(group.id)}/members/${seg(member.id)}`);
    note(result.ok ? `${member.name} was removed.` : messageFor(result.error), !result.ok);
    await load();
  });
  return h('li', {}, h('span', { class: 'grow' }, member.name, h('br'), h('small', {}, member.email)), remove);
}

function addForm(group) {
  const id = `add-${group.id}`;
  const input = h('input', { type: 'email', id, autocomplete: 'off', required: true });
  const form = h(
    'form',
    { class: 'row' },
    h('div', { class: 'field' }, h('label', { for: id }, `Add a person to ${group.name} by email`), input),
    h('button', { type: 'submit' }, 'Add'),
  );
  form.addEventListener('submit', async (event) => {
    event.preventDefault();
    note('', false);
    const result = await api('POST', `/groups/${seg(group.id)}/members`, { email: input.value.trim() });
    note(result.ok ? 'Added.' : messageFor(result.error), !result.ok);
    await load();
  });
  return form;
}

async function deleteGroup(group) {
  note('', false);
  const result = await api('DELETE', `/groups/${seg(group.id)}`);
  if (!result.ok) return note(messageFor(result.error), true);
  if (groupId) location.assign('/groups.html');
  else {
    note(`${group.name} was deleted.`, false);
    await load();
  }
}
