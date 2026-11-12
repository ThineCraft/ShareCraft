// A library or a folder: its breadcrumb, its contents a page at a time, and what a person may
// do with each item. The server decides who may do what; this page only offers what the roles
// it was told allow and shows the server's refusals as plain sentences.

import { EDITORS, withExtension } from './editor-host.js';
import {
  api,
  can,
  canMoveHere,
  formatTime,
  goToSignIn,
  h,
  itemActions,
  messageFor,
  moveChoices,
  pageFor,
  queryId,
  requireMe,
  roleLine,
  seg,
  setupTopbar,
  showCrumbs,
  showMessage,
  showSiteNav,
} from './common.js';

const PAGE = 50;
const $ = (id) => document.getElementById(id);
const error = $('error');
const MISSING = 'This folder does not exist, or you do not have access to it.';

const me = await requireMe();
setupTopbar(me, error);
const nodeId = queryId(location.search);
if (!nodeId) fail(MISSING);
const here = await showCrumbs($('crumbs'), nodeId, { linkLast: false });
if (!here) fail(MISSING);
const self = here.path[here.path.length - 1];
if (self.kind === 'site' || self.kind === 'document') {
  location.replace(pageFor(self.kind, self.id));
  await new Promise(() => {});
}
$('title').textContent = self.name;
document.title = `${self.name} — ShareCraft`;
$('kind').textContent =
  (self.kind === 'library' ? 'Library' : 'Folder') +
  (here.access ? ` · ${roleLine(here.access)}` : ' · Limited access');
$('edit-tools').hidden = !can(here.access, 'add');
$('top-tools').hidden = !can(here.access, 'manage_access');
$('explorer-link').href = `/explorer.html?root=${seg(nodeId)}`;
$('access-link').href = `/access.html?id=${seg(nodeId)}`;
showSiteNav($('sitenav'), here.path[0].id, here.path.find((step) => step.kind === 'library')?.id);

let items = [];
let next = null;

$('more').addEventListener('click', () => loadPage());
await reload();

// ---- the listing ----

async function reload() {
  items = [];
  next = null;
  await loadPage();
}

async function loadPage() {
  const query = `limit=${PAGE}${next ? `&after=${encodeURIComponent(next)}` : ''}`;
  const result = await api('GET', `/nodes/${seg(nodeId)}/children?${query}`);
  if (!result.ok) {
    note(messageFor(result.error), true);
    return;
  }
  items = items.concat(result.data.items);
  next = result.data.next;
  render();
}

function render() {
  $('empty').hidden = items.length > 0;
  $('more').hidden = !next;
  $('items').replaceChildren(...items.map(row));
}

function kindLabel(item) {
  if (item.kind === 'folder') return 'Folder';
  if (item.kind === 'library') return 'Library';
  return Object.hasOwn(EDITORS, item.editor) ? EDITORS[item.editor].name : 'File';
}

function row(item) {
  const buttons = [];
  const allowed = itemActions(item);
  if (allowed.rename) buttons.push(action('Rename', item, askRename));
  if (allowed.move) buttons.push(action('Move…', item, askMove));
  if (allowed.remove) buttons.push(action('Delete', item, askDelete));
  if (allowed.access) {
    buttons.push(
      h(
        'a',
        {
          class: 'button plain',
          href: `/access.html?id=${seg(item.id)}`,
          'aria-label': `Access to ${item.name}`,
        },
        'Access',
      ),
    );
  }
  const saved =
    item.kind === 'document'
      ? item.revised_at
        ? `Last saved ${formatTime(item.revised_at)}`
        : 'Not saved yet'
      : null;
  return h(
    'li',
    {},
    h('span', { class: 'grow' }, h('a', { href: pageFor(item.kind, item.id, item.editor) }, item.name)),
    h('small', {}, kindLabel(item)),
    saved && h('small', {}, saved),
    item.access ? null : h('small', { class: 'mark' }, 'Limited access'),
    item.inherits || item.kind === 'library' ? null : h('small', { class: 'mark' }, 'Own permissions'),
    buttons.length ? h('span', { class: 'actions' }, ...buttons) : null,
  );
}

function action(label, item, handler) {
  const button = h(
    'button',
    { type: 'button', class: 'plain', 'aria-label': `${label} ${item.name}` },
    label,
  );
  button.addEventListener('click', () => handler(item));
  return button;
}

function note(text, isError) {
  error.classList.toggle('ok', !isError);
  showMessage(error, text);
}

// ---- creating ----

$('new-folder').addEventListener('submit', async (event) => {
  event.preventDefault();
  note('', false);
  const result = await api('POST', `/nodes/${seg(nodeId)}/children`, {
    kind: 'folder',
    name: $('folder-name').value.trim(),
  });
  if (!result.ok) return note(messageFor(result.error), true);
  $('folder-name').value = '';
  note(`Folder “${result.data.name}” was created.`, false);
  await reload();
});

$('new-doc').addEventListener('submit', async (event) => {
  event.preventDefault();
  note('', false);
  const editor = $('doc-kind').value;
  const name = $('doc-name').value.trim();
  const extension = Object.hasOwn(EDITORS, editor) ? EDITORS[editor].extension : null;
  const body = { parent: nodeId, editor };
  if (extension) body.name = withExtension(name, extension);
  else if (name) body.name = name;
  const result = await api('POST', '/docs', body);
  if (result.ok) location.assign(pageFor('document', result.data.id, result.data.editor));
  else note(result.data?.message ?? messageFor(result.error), true);
});

$('upload-doc').addEventListener('submit', async (event) => {
  event.preventDefault();
  note('', false);
  const file = $('upload-file').files[0];
  if (!file) return;
  let response;
  try {
    response = await fetch(
      `/api/docs/upload?parent=${seg(nodeId)}&name=${encodeURIComponent(file.name)}`,
      {
        method: 'POST',
        credentials: 'same-origin',
        headers: { 'Content-Type': 'application/octet-stream' },
        body: file,
      },
    );
  } catch {
    return note(messageFor('network'), true);
  }
  const data = await response.json().catch(() => null);
  if (response.ok && data?.id) {
    $('upload-doc').reset();
    note(`“${data.name ?? file.name}” was uploaded.`, false);
    await reload();
    return;
  }
  const code = typeof data?.error === 'string' ? data.error : 'unknown';
  if (response.status === 401 && code === 'not_signed_in') goToSignIn();
  note(uploadProblem(response.status, code), true);
});

function uploadProblem(status, code) {
  switch (status) {
    case 400:
      return 'That file cannot be uploaded. Upload a .xlsx, .docx, .pdf or .solidcraft.json file that is not empty.';
    case 413:
      return 'That file is too large to upload.';
    case 422:
      return 'That file name is not allowed. Rename the file and try again.';
    default:
      return messageFor(code);
  }
}

// ---- rename ----

let renaming = null;
const renameDialog = $('rename-dialog');
$('rename-cancel').addEventListener('click', () => renameDialog.close());

function askRename(item) {
  renaming = item;
  $('rename-title').textContent = `Rename “${item.name}”`;
  showMessage($('rename-error'), '');
  const extension = item.kind === 'document' ? extensionOf(item.name) : '';
  $('rename-hint').textContent = extension
    ? `Keep the ending ${extension}, which opens the file in its editor.`
    : '';
  $('rename-name').value = item.name;
  renameDialog.showModal();
  $('rename-name').setSelectionRange(0, item.name.length - extension.length);
  $('rename-name').focus();
}

function extensionOf(name) {
  for (const editor of Object.values(EDITORS)) {
    const ext = editor.extension;
    if (ext && name.toLowerCase().endsWith(ext)) return name.slice(name.length - ext.length);
  }
  return '';
}

$('rename-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  const item = renaming;
  const name = $('rename-name').value.trim();
  const result = await api('PATCH', `/nodes/${seg(item.id)}`, { name });
  if (!result.ok) return showMessage($('rename-error'), messageFor(result.error));
  renameDialog.close();
  note(`Renamed to “${result.data.name}”.`, false);
  await reload();
});

// ---- delete ----

let deleting = null;
const deleteDialog = $('delete-dialog');
$('delete-cancel').addEventListener('click', () => deleteDialog.close());

function askDelete(item) {
  deleting = item;
  $('delete-title').textContent = `Delete “${item.name}”?`;
  $('delete-text').textContent =
    item.kind === 'folder'
      ? `The folder “${item.name}” will be deleted. It must be empty.`
      : `The document “${item.name}” will be deleted. Anyone who has it open is disconnected.`;
  showMessage($('delete-error'), '');
  deleteDialog.showModal();
}

$('delete-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  const item = deleting;
  const result = await api('DELETE', `/nodes/${seg(item.id)}`);
  if (!result.ok) return showMessage($('delete-error'), messageFor(result.error));
  deleteDialog.close();
  note(`“${item.name}” was deleted.`, false);
  await reload();
});

// ---- move ----

const moveDialog = $('move-dialog');
let moving = null;
let trail = [];
let moveRequest = 0; // the latest listing asked for; older answers are dropped
$('move-cancel').addEventListener('click', () => moveDialog.close());

function askMove(item) {
  moving = item;
  trail = here.path.map(({ id, kind, name, access }) => ({ id, kind, name, access }));
  $('move-title').textContent = `Move “${item.name}”`;
  showMessage($('move-error'), '');
  moveDialog.showModal();
  browse();
}

// Lists the places inside the last step of `trail` (the sites at the top).
async function browse() {
  const request = ++moveRequest;
  const place = trail[trail.length - 1];
  let listed = [];
  let failure = null;
  if (!place) {
    const result = await api('GET', '/sites');
    if (result.ok) listed = result.data.map((site) => ({ ...site, kind: 'site' }));
    else failure = result.error;
  } else {
    let after = null;
    do {
      const query = `limit=500${after ? `&after=${encodeURIComponent(after)}` : ''}`;
      const result = await api('GET', `/nodes/${seg(place.id)}/children?${query}`);
      if (!result.ok) {
        failure = result.error;
        break;
      }
      listed = listed.concat(result.data.items);
      after = result.data.next;
    } while (after);
  }
  if (request !== moveRequest) return;
  showMessage($('move-error'), failure ? messageFor(failure) : '');
  const places = moveChoices(listed, moving.id);
  $('move-empty').hidden = places.length > 0 || failure !== null;
  $('move-list').replaceChildren(
    ...places.map((choice) => {
      const open = h('button', { type: 'button', class: 'plain' }, choice.name);
      open.addEventListener('click', () => {
        trail = trail.concat([
          { id: choice.id, kind: choice.kind, name: choice.name, access: choice.access },
        ]);
        browse();
      });
      const label = { site: 'Site', library: 'Library', folder: 'Folder' }[choice.kind];
      return h(
        'li',
        {},
        h('span', { class: 'grow' }, open),
        h('small', {}, label),
        choice.access || choice.kind === 'site' ? null : h('small', { class: 'mark' }, 'Limited access'),
      );
    }),
  );
  showTrail();
}

function showTrail() {
  const top = h('button', { type: 'button', class: 'plain' }, 'All sites');
  top.addEventListener('click', () => {
    trail = [];
    browse();
  });
  const steps = trail.map((step, at) => {
    const button = h('button', { type: 'button', class: 'plain' }, step.name);
    button.addEventListener('click', () => {
      trail = trail.slice(0, at + 1);
      browse();
    });
    return button;
  });
  $('move-crumbs').replaceChildren(
    h('ol', {}, ...[top, ...steps].map((button) => h('li', {}, button))),
  );
  $('move-here').disabled = !canMoveHere(trail, moving.id, nodeId);
}

$('move-here').addEventListener('click', async () => {
  const item = moving;
  const place = trail[trail.length - 1];
  if (!canMoveHere(trail, item.id, nodeId)) return;
  $('move-here').disabled = true;
  const result = await api('PATCH', `/nodes/${seg(item.id)}`, { parent: place.id });
  if (!result.ok) {
    showMessage($('move-error'), messageFor(result.error));
    $('move-here').disabled = !canMoveHere(trail, item.id, nodeId);
    return;
  }
  moveDialog.close();
  note(`“${item.name}” was moved to “${place.name}”.`, false);
  await reload();
});

function fail(text) {
  showMessage(error, text);
  throw new Error(text);
}
