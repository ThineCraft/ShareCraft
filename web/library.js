// A library or a folder: its breadcrumb, its contents a page at a time, and what a person may
// do with each item. The server decides who may do what; this page only offers what the roles
// it was told allow and shows the server's refusals as plain sentences.

import { EDITORS, withExtension } from './editor-host.js';
import {
  api,
  bulkSummary,
  can,
  canPlaceHere,
  formatTime,
  h,
  itemActions,
  messageFor,
  offerRequest,
  moveChoices,
  pageFor,
  queryId,
  requireMe,
  roleLine,
  selectionState,
  seg,
  setupTopbar,
  showCrumbs,
  showMessage,
  showSiteNav,
} from './common.js';
import {
  CONFLICT_CHOICES,
  UploadQueue,
  conflictQuestion,
  filesFromDrop,
  filesFromPicker,
  outcomeNote,
  progressOf,
  progressText,
  uploadsSummary,
} from './upload.js';

const BULK_MAX = 500;

const PAGE = 50;
const $ = (id) => document.getElementById(id);
const error = $('error');
const MISSING = 'This folder does not exist, or you do not have access to it.';

const me = await requireMe();
setupTopbar(me, error);
const nodeId = queryId(location.search);
if (!nodeId) fail(MISSING);
const here = await showCrumbs($('crumbs'), nodeId, { linkLast: false });
if (!here) {
  offerRequest(error, nodeId);
  fail(MISSING);
}
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

$('bin-link').href = `/bin.html?site=${seg(here.path[0].id)}`;
const siteId = here.path[0].id;
const canAdd = can(here.access, 'add');

// Everything the code below reads while the page is drawn is declared here, before the first
// await: a `let` or `const` further down would still be unusable when `reload` draws the list.
let items = [];
let next = null;
const selected = new Set(); // ids of the checked items
const uploads = []; // every upload shown in the panel, as the queue reports it
let uploadChain = Promise.resolve(); // uploads run one batch at a time
let moving = []; // the items the move or copy dialog is for
let moveMode = 'move';
let trail = [];
let moveRequest = 0; // the latest listing asked for; older answers are dropped
let deleting = [];
let renaming = null;
const uploadRows = new Map(); // outcome -> its list item
const dirtyUploads = new Set(); // outcomes to redraw at the next frame
let drawScheduled = false;

$('more').addEventListener('click', () => loadPage());
$('select-all').addEventListener('change', () => {
  const all = $('select-all').checked;
  for (const item of items) {
    if (all) selected.add(item.id);
    else selected.delete(item.id);
  }
  render();
});
await reload();

// ---- the listing ----

async function reload() {
  items = [];
  next = null;
  selected.clear();
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
  showSelection();
}

/** The bulk bar: Select all, how many are checked, and the buttons that need a selection. */
function showSelection() {
  const choosable = items.filter((item) => item.kind !== 'library');
  const state = selectionState(selected, choosable.map((item) => item.id));
  $('bulk-bar').hidden = choosable.length === 0;
  $('select-all').checked = state.all;
  $('select-all').indeterminate = state.some;
  $('selected-count').textContent = state.count === 0 ? '' : `${state.count} selected`;
  $('select-note').hidden = !next;
  $('select-note').textContent = `Select all covers the ${choosable.length} items loaded so far. Use Show more to load the rest.`;
  const allowed = selectedItems().map(itemActions);
  $('bulk-move').disabled = state.count === 0 || !allowed.every((a) => a.move);
  $('bulk-copy').disabled = state.count === 0;
  $('bulk-delete').disabled = state.count === 0 || !allowed.every((a) => a.remove);
}

function selectedItems() {
  return items.filter((item) => selected.has(item.id));
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
  if (allowed.move) buttons.push(action('Move…', item, (one) => askMove([one], 'move')));
  if (allowed.remove) buttons.push(action('Delete', item, (one) => askDelete([one])));
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
  const box =
    item.kind === 'library'
      ? null
      : h('input', { type: 'checkbox', 'aria-label': `Select ${item.name}`, checked: selected.has(item.id) });
  box?.addEventListener('change', () => {
    if (box.checked) selected.add(item.id);
    else selected.delete(item.id);
    showSelection();
  });
  return h(
    'li',
    {},
    box,
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

// ---- uploading ----

$('pick-files').addEventListener('change', (event) => {
  startUploads(filesFromPicker(event.target.files));
  event.target.value = '';
});
$('pick-folder').addEventListener('change', (event) => {
  startUploads(filesFromPicker(event.target.files));
  event.target.value = '';
});
$('uploads-clear').addEventListener('click', () => {
  for (let at = uploads.length - 1; at >= 0; at -= 1) {
    if (['waiting', 'hashing', 'uploading'].includes(uploads[at].status)) continue;
    forgetUpload(uploads[at]);
  }
  drawUploads();
});

// Files and folders dropped anywhere on the page go into this library or folder.
const hasFiles = (event) => [...(event.dataTransfer?.types ?? [])].includes('Files');
document.addEventListener('dragover', (event) => {
  if (!canAdd || !hasFiles(event)) return;
  event.preventDefault();
  document.body.classList.add('dragging');
});
document.addEventListener('dragleave', (event) => {
  if (event.relatedTarget === null) document.body.classList.remove('dragging');
});
document.addEventListener('drop', async (event) => {
  document.body.classList.remove('dragging');
  if (!canAdd || !hasFiles(event)) return;
  event.preventDefault();
  startUploads(await filesFromDrop(event.dataTransfer));
});

/** Queues `files` (from a drop or a picker) behind any upload still running. */
function startUploads(files) {
  if (files.length === 0) return;
  note('', false);
  const queue = new UploadQueue({
    parent: nodeId,
    site: siteId,
    onChange: (outcome) => {
      if (!uploads.includes(outcome)) uploads.push(outcome);
      dirtyUploads.add(outcome);
      scheduleDraw();
    },
    resolve: askConflict,
  });
  uploadChain = uploadChain
    .then(() => queue.run(files))
    .then(() => reload())
    .catch(() => {});
}

/** Redraws the panel at the next frame, once, however many changes came in (chunks arrive fast). */
function scheduleDraw() {
  if (drawScheduled) return;
  drawScheduled = true;
  const later = globalThis.requestAnimationFrame ?? ((fn) => setTimeout(fn, 50));
  later(() => {
    drawScheduled = false;
    drawUploads();
  });
}

/** Draws only the rows that changed; a row whose words are the same just moves its bar. */
function drawUploads() {
  $('upload-panel').hidden = uploads.length === 0;
  for (const outcome of dirtyUploads) {
    if (!uploads.includes(outcome)) continue;
    const row = uploadRows.get(outcome);
    if (!row) {
      const made = uploadRow(outcome);
      uploadRows.set(outcome, made);
      $('uploads').append(made.item);
    } else if (row.key !== uploadKey(outcome)) {
      const made = uploadRow(outcome);
      row.item.replaceChildren(...made.item.childNodes);
      uploadRows.set(outcome, { ...made, item: row.item });
    } else {
      const bar = progressOf(outcome);
      if (bar && row.bar) {
        row.bar.max = bar.max;
        row.bar.value = bar.value;
      }
      if (row.text) row.text.textContent = uploadText(outcome);
    }
  }
  dirtyUploads.clear();
  // Announced when the counts change, not for every chunk.
  const summary = uploadsSummary(uploads);
  if ($('upload-summary').textContent !== summary) $('upload-summary').textContent = summary;
}

function forgetUpload(outcome) {
  uploads.splice(uploads.indexOf(outcome), 1);
  uploadRows.get(outcome)?.item.remove();
  uploadRows.delete(outcome);
}

/** What decides whether a row must be built again: anything but the bytes sent so far. */
const uploadKey = (outcome) => `${outcome.status}|${outcome.message}|${outcome.retry}|${outcome.name}`;

function uploadText(outcome) {
  if (outcome.status === 'waiting') return 'Waiting';
  if (outcome.status === 'hashing') return `Checking the file… ${progressText(outcome.hashed ?? 0, outcome.size)}`;
  if (outcome.status === 'uploading') return progressText(outcome.sent, outcome.size);
  return outcomeNote(outcome);
}

function uploadRow(outcome) {
  const path = outcome.file.path?.length ? `${outcome.file.path.join(' / ')} / ` : '';
  const text = h('small', {}, uploadText(outcome));
  const progress = progressOf(outcome);
  const bar =
    progress && progress.max > 0
      ? h('progress', { max: progress.max, value: progress.value, 'aria-label': `Progress of ${outcome.name}` })
      : null;
  const again = outcome.retry
    ? h('button', { type: 'button', class: 'plain' }, outcome.keep ? 'Try again' : 'Upload again')
    : null;
  again?.addEventListener('click', () => {
    forgetUpload(outcome);
    // A kept session only needs its finish sent again; otherwise the file goes up afresh.
    startUploads([
      outcome.keep
        ? { ...outcome.file, resumeId: outcome.sessionId, resumeChoice: outcome.choice, parentId: outcome.parentId }
        : outcome.file,
    ]);
  });
  const item = h(
    'li',
    {},
    h('span', { class: 'grow' }, h('strong', {}, `${path}${outcome.name}`), h('br'), text),
    bar,
    outcome.status === 'failed' ? h('small', { class: 'mark' }, 'Not uploaded') : null,
    again,
  );
  return { item, text, bar, key: uploadKey(outcome) };
}

// The question asked when a name is taken: Replace, Keep both or Skip, for this file or the rest.
const conflictDialog = $('conflict-dialog');
function askConflict({ name, suggested }) {
  return new Promise((resolve) => {
    $('conflict-text').textContent = conflictQuestion(name, suggested);
    $('conflict-all').checked = false;
    let choice = 'skip'; // Escape skips this file only
    let clicked = false;
    for (const { value } of CONFLICT_CHOICES) {
      const id = { replace: 'conflict-replace', keep_both: 'conflict-keep', skip: 'conflict-skip' }[value];
      $(id).onclick = () => {
        choice = value;
        clicked = true;
        conflictDialog.close();
      };
    }
    // "Do this for the rest" counts only when a button was clicked, never for Escape.
    conflictDialog.addEventListener(
      'close',
      () => resolve({ choice, all: clicked && $('conflict-all').checked }),
      { once: true },
    );
    conflictDialog.showModal();
  });
}

// ---- rename ----

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

// ---- bulk: delete, move and copy ----

const deleteDialog = $('delete-dialog');
$('delete-cancel').addEventListener('click', () => deleteDialog.close());
$('bulk-delete').addEventListener('click', () => askDelete(selectedItems()));
$('bulk-move').addEventListener('click', () => askMove(selectedItems(), 'move'));
$('bulk-copy').addEventListener('click', () => askMove(selectedItems(), 'copy'));

function askDelete(list) {
  deleting = list;
  const one = list[0];
  $('delete-title').textContent = list.length === 1 ? `Delete “${one.name}”?` : `Delete ${list.length} items?`;
  const inside = list.some((item) => item.kind === 'folder') ? ' Folders go with everything inside them.' : '';
  $('delete-text').textContent =
    `${list.length === 1 ? `“${one.name}” will be` : `${list.length} items will be`} moved to the recycle bin, where ${
      list.length === 1 ? 'it' : 'they'
    } can be restored for 93 days.${inside} Anyone who has a deleted file open is disconnected.`;
  showMessage($('delete-error'), '');
  deleteDialog.showModal();
}

$('delete-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  const list = deleting;
  deleteDialog.close();
  await runBulk('delete', list, null);
});

/** Sends `list` through `POST /api/bulk` (500 at a time) and shows what happened to each. */
async function runBulk(op, list, parent) {
  note('', false);
  const results = [];
  for (let at = 0; at < list.length; at += BULK_MAX) {
    const batch = list.slice(at, at + BULK_MAX);
    const body = { op, items: batch.map((item) => item.id) };
    if (parent) body.parent = parent;
    const result = await api('POST', '/bulk', body);
    if (!result.ok) {
      for (const item of batch) results.push({ id: item.id, ok: false, error: result.error });
    } else results.push(...result.data);
  }
  const names = Object.fromEntries(list.map((item) => [item.id, item.name]));
  const summary = bulkSummary(op, results, names);
  $('bulk-headline').textContent = summary.headline;
  $('bulk-results-box').hidden = false;
  $('bulk-results').hidden = false;
  $('bulk-results').replaceChildren(
    ...summary.lines.map((line) =>
      h(
        'li',
        {},
        h('span', { class: 'grow' }, h('strong', {}, line.name), h('br'), h('small', {}, line.text)),
        line.ok ? null : h('small', { class: 'mark' }, 'Not done'),
      ),
    ),
  );
  await reload();
}

// ---- move and copy (one dialog) ----

const moveDialog = $('move-dialog');
$('move-cancel').addEventListener('click', () => moveDialog.close());

function askMove(list, mode) {
  moving = list;
  moveMode = mode;
  trail = here.path.map(({ id, kind, name, access }) => ({ id, kind, name, access }));
  const verb = mode === 'copy' ? 'Copy' : 'Move';
  $('move-title').textContent = list.length === 1 ? `${verb} “${list[0].name}”` : `${verb} ${list.length} items`;
  $('move-here').textContent = `${verb} here`;
  showMessage($('move-error'), '');
  moveDialog.showModal();
  browse();
}

const movingIds = () => moving.map((item) => item.id);

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
  const places = moveChoices(listed, movingIds());
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
  $('move-here').disabled = !canPlaceHere(trail, movingIds(), nodeId, { copy: moveMode === 'copy' });
}

$('move-here').addEventListener('click', async () => {
  const list = moving;
  const mode = moveMode;
  const place = trail[trail.length - 1];
  if (!canPlaceHere(trail, movingIds(), nodeId, { copy: mode === 'copy' })) return;
  moveDialog.close();
  await runBulk(mode, list, place.id);
});

function fail(text) {
  showMessage(error, text);
  throw new Error(text);
}
