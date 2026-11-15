// A library or a folder: its breadcrumb, its contents a page at a time, and what a person may
// do with each item. The server decides who may do what; this page only offers what the roles
// it was told allow and shows the server's refusals as plain sentences.

import { EDITORS, newDocEditors, withExtension } from './editor-host.js';
import {
  api,
  bulkSummary,
  can,
  canPlaceHere,
  formatDate,
  formatSize,
  h,
  itemActions,
  itemMarks,
  libraryKindLabel,
  libraryViews,
  messageFor,
  newMenu,
  policyRefusalText,
  rowMenu,
  offerRequest,
  moveChoices,
  pageFor,
  queryId,
  requireMe,
  roleLine,
  selectionState,
  seg,
  setupTopbar,
  showThumbnail,
  showCrumbs,
  showMessage,
  siteChrome,
  typeListText,
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
import { setupMenu } from './menu.js';
import { addPaletteActions } from './palette.js';
import { fileIcon, fileKind, nextSort, parseSort, relativeTime, sortDescription, sortEntries, typeLabel } from './files.js';

const BULK_MAX = 500;
const GRID_PAGE = 200; // the grid's page, the most the server answers at once

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
// The library this place is in, with its kind and settings (a folder is judged by its library).
const libraryStep = here.path.find((step) => step.kind === 'library');
const libraryInfo = libraryStep ? await api('GET', `/libraries/${seg(libraryStep.id)}`) : null;
const library = libraryInfo?.ok ? libraryInfo.data : null;
$('kind').textContent =
  (self.kind === 'library' ? libraryKindLabel(library?.kind) : 'Folder') +
  (here.access ? ` · ${roleLine(here.access)}` : ' · Limited access');
if (library?.description) {
  $('library-description').textContent = library.description;
  $('library-description').hidden = false;
}
// New document offers only the editors whose files the library takes: none in an image library.
const docEditors = newDocEditors(library?.effective_allowed_types ?? null, library?.blocked_types ?? []);
// What the library takes, so that a refusal is no surprise.
const accepts = [];
if (Array.isArray(library?.effective_allowed_types)) accepts.push(`Takes only ${typeListText(library.effective_allowed_types)}.`);
if (library?.blocked_types?.length) accepts.push(`Does not take ${typeListText(library.blocked_types)}.`);
if (typeof library?.max_file_bytes === 'number') accepts.push(`Files up to ${formatSize(library.max_file_bytes)}.`);
$('accepts').textContent = accepts.join(' ');
$('drop-accepts').textContent = accepts.join(' ');
const manageSite = can(here.access, 'manage_site') && libraryStep !== undefined;
// The library-level links live in the More menu; an entry the person may not use is left out.
const showEntry = (id, shown) => {
  $(id).hidden = !shown;
  $(id).closest('li').hidden = !shown;
};
showEntry('settings-link', manageSite);
if (libraryStep) $('settings-link').href = `/library-settings.html?id=${seg(libraryStep.id)}`;
showEntry('access-link', can(here.access, 'manage_access'));
showEntry('explorer-link', can(here.access, 'manage_access'));
$('explorer-link').href = `/explorer.html?root=${seg(nodeId)}`;
$('access-link').href = `/access.html?id=${seg(nodeId)}`;
await siteChrome(here.path[0].id, { currentId: here.path.find((step) => step.kind === 'library')?.id, footer: true });

$('bin-link').href = `/bin.html?site=${seg(here.path[0].id)}`;
const siteId = here.path[0].id;
const canAdd = can(here.access, 'add');
$('accepts').hidden = !canAdd || accepts.length === 0;
$('drop-target').textContent = self.name;
setupMenu($('more-button'), $('more-menu'));

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
const views = libraryViews(library?.kind);
let view = 'list'; // 'grid' only in an image library
let watcher = null; // loads a tile's thumbnail when it scrolls into view
let templates = [];
let creating = null; // what the create dialog makes: 'folder', 'doc:<editor>' or 'template'
let actionsFor = null; // the item whose Actions menu was used last, so focus can go back to it
let sort = loadSort(); // the list's order, remembered per viewer

$('more').addEventListener('click', () => loadPage());
setupViews();
setupSort();
// "5 minutes ago" moves on while the page stays open.
setInterval(refreshTimes, 60_000);
await setupTemplates();
setupNewMenu();
// The command palette offers the same uploads and New folder as the New menu, for people who may add.
addPaletteActions(() => {
  if (!canAdd) return [];
  const actions = [{ key: 'upload-files', label: 'Upload files', detail: `To ${self.name}`, keywords: 'add files', run: () => $('pick-files').click() }];
  if (library?.folders !== false) actions.push({ key: 'new-folder', label: 'New folder', detail: `In ${self.name}`, keywords: 'create add make folder', run: () => askCreate('folder') });
  return actions;
});
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
  const grid = view === 'grid';
  const query = `limit=${grid ? GRID_PAGE : PAGE}${grid ? '&view=grid' : ''}${next ? `&after=${encodeURIComponent(next)}` : ''}`;
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
  watcher?.disconnect();
  watcher = null;
  const grid = view === 'grid';
  const lostFocus =
    document.activeElement === document.body ||
    $('items').contains(document.activeElement) ||
    $('file-rows').contains(document.activeElement);
  // The grid of an image library is a list of tiles; the list is a table that sorts.
  $('items').classList.toggle('grid', grid);
  $('items').hidden = !grid;
  $('file-table').hidden = grid || items.length === 0;
  $('items').replaceChildren(...(grid ? items.map(tile) : []));
  $('file-rows').replaceChildren(...(grid ? [] : sortEntries(items, sort.key, sort.direction).map(row)));
  showSort();
  showSelection();
  // After a rename (or anything else chosen from a row's menu) focus goes back to that row's
  // Actions button, when the row is still there and focus has not gone anywhere else.
  if (lostFocus && actionsFor) {
    const again = [...document.querySelectorAll('#items button[data-for], #file-rows button[data-for]')].find(
      (b) => b.dataset.for === actionsFor,
    );
    again?.focus();
  }
}

// ---- sorting the list ----

function loadSort() {
  try {
    return parseSort(localStorage.getItem('sharecraft-library-sort'));
  } catch {
    return parseSort(null); // no storage: by name
  }
}

/** The column headers sort on wide screens; a Sort by list does the same on narrow ones. */
function setupSort() {
  for (const button of $('file-table').querySelectorAll('button.sort')) {
    button.addEventListener('click', () => sortBy(nextSort(sort, button.dataset.sort)));
  }
  $('sort-select').addEventListener('change', () => {
    const [key, direction] = $('sort-select').value.split(':');
    sortBy(parseSort({ key, direction }));
  });
}

function sortBy(chosen) {
  sort = chosen;
  try {
    localStorage.setItem('sharecraft-library-sort', JSON.stringify(sort));
  } catch {
    // not remembered; the list is still sorted
  }
  render();
  $('sort-status').textContent = sortDescription(sort);
}

/** Marks the sorted column (aria-sort and its arrow) and says when only part is loaded. */
function showSort() {
  for (const th of $('file-table').querySelectorAll('th[data-sort]')) {
    const on = th.dataset.sort === sort.key;
    if (on) th.setAttribute('aria-sort', sort.direction);
    else th.removeAttribute('aria-sort');
    th.querySelector('.sort-arrow').textContent = on ? (sort.direction === 'ascending' ? '▲' : '▼') : '';
  }
  $('sort-narrow').hidden = $('file-table').hidden;
  $('sort-select').value = `${sort.key}:${sort.direction}`;
  const partial = view === 'list' && next !== null && (sort.key !== 'name' || sort.direction !== 'ascending');
  $('sort-note').hidden = !partial;
  $('sort-note').textContent = partial
    ? `Sorted among the ${items.length} items loaded so far. Show more loads the rest.`
    : '';
}

/** Rewrites every "5 minutes ago" from the time it stands for. */
function refreshTimes() {
  for (const time of $('file-rows').querySelectorAll('time[data-at]')) {
    time.textContent = relativeTime(time.dataset.at);
  }
}

// ---- the Grid / List switch of an image library ----

function setupViews() {
  $('view-switch').hidden = views.length < 2;
  if (views.length < 2) return;
  try {
    if (localStorage.getItem('sharecraft-library-view') === 'grid') view = 'grid';
  } catch {
    // no storage: the list is shown
  }
  for (const [id, which] of [['view-list', 'list'], ['view-grid', 'grid']]) {
    $(id).addEventListener('click', async () => {
      if (view === which) return;
      view = which;
      try {
        localStorage.setItem('sharecraft-library-view', which);
      } catch {
        // not remembered; nothing else depends on it
      }
      showViewButtons();
      await reload();
    });
  }
  showViewButtons();
}

function showViewButtons() {
  $('view-list').setAttribute('aria-pressed', String(view === 'list'));
  $('view-grid').setAttribute('aria-pressed', String(view === 'grid'));
}

/** A tile of the grid: the thumbnail (loaded when it comes into view), the name and its marks. */
function tile(item) {
  const picture = h('div', { class: 'thumb', 'aria-hidden': 'true' });
  if (item.thumbnail) {
    picture.dataset.id = item.id;
    watchThumbnail(picture);
  } else {
    picture.append(placeholder(item));
  }
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
    h('a', { href: pageFor(item.kind, item.id, item.editor), class: 'tile-link' }, picture, h('span', { class: 'tile-name' }, item.name)),
    h('span', { class: 'tile-foot' }, box, ...itemMarks(item).map((mark) => h('small', { class: 'mark warn' }, mark))),
  );
}

/** The plain placeholder of a tile with no thumbnail: what kind of item it is. */
function placeholder(item) {
  return h('span', { class: 'placeholder' }, item.kind === 'document' ? 'No thumbnail' : kindLabel(item));
}

function watchThumbnail(picture) {
  if (!('IntersectionObserver' in globalThis)) {
    showThumbnail(picture, thumbnailUrl(picture), thumbnailOptions(picture));
    return;
  }
  watcher ??= new IntersectionObserver(
    (entries) => {
      for (const entry of entries) {
        if (!entry.isIntersecting) continue;
        watcher?.unobserve(entry.target);
        showThumbnail(entry.target, thumbnailUrl(entry.target), thumbnailOptions(entry.target));
      }
    },
    { rootMargin: '200px' },
  );
  watcher.observe(picture);
}

/** The thumbnail route of a tile's box (a declaration: tiles are drawn before this line runs). */
function thumbnailUrl(picture) {
  return `/api/docs/${seg(picture.dataset.id)}/thumbnail`;
}

/** A row's picture falls back to its type icon; a tile's to the placeholder words. */
function thumbnailOptions(picture) {
  const kind = picture.dataset.icon;
  return kind ? { fallback: () => fileIcon(kind) } : {};
}

// ---- New from template ----

async function setupTemplates() {
  if (!canAdd || !libraryStep) return;
  const result = await api('GET', `/libraries/${seg(libraryStep.id)}/templates`);
  templates = result.ok ? result.data : [];
  $('create-template').replaceChildren(
    ...templates.map((template) =>
      h('option', { value: template.id }, `${template.name} (${template.library_name})`),
    ),
  );
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

/** A row of the file table: select box, icon and name, modified, type, sharing and Actions. */
function row(item) {
  const kind = fileKind(item.name, item.editor, item.kind);
  const box =
    item.kind === 'library'
      ? null
      : h('input', { type: 'checkbox', 'aria-label': `Select ${item.name}`, checked: selected.has(item.id) });
  box?.addEventListener('change', () => {
    if (box.checked) selected.add(item.id);
    else selected.delete(item.id);
    showSelection();
  });
  // The icon; for an image its thumbnail once it scrolls into view (the icon until then, and
  // again should no thumbnail come).
  const picture = h('span', { class: 'file-pic', 'aria-hidden': 'true' }, fileIcon(kind));
  if (item.thumbnail) {
    picture.dataset.id = item.id;
    picture.dataset.icon = kind;
    watchThumbnail(picture);
  }
  const type = typeLabel(item);
  const modified = modifiedCell(item);
  return h(
    'tr',
    {},
    h('td', { class: 'c-select' }, box),
    h(
      'td',
      { class: 'c-name' },
      h(
        'div',
        { class: 'name-cell' },
        picture,
        h(
          'div',
          { class: 'name-text' },
          h('a', { href: pageFor(item.kind, item.id, item.editor), class: 'name-link' }, item.name),
          ...itemMarks(item).map((mark) => h('small', { class: 'mark warn' }, mark)),
          // The narrow layout's second line; screen readers get the same from the cells.
          h(
            'span',
            { class: 'sub', 'aria-hidden': 'true' },
            h('span', {}, type),
            modified.short ? h('span', {}, modified.short) : null,
            ...sharingMarks(item),
          ),
        ),
      ),
    ),
    h('td', { class: 'c-modified' }, ...modified.nodes),
    h('td', { class: 'c-type' }, type),
    h('td', { class: 'c-sharing' }, ...sharingMarks(item)),
    h('td', { class: 'c-actions' }, actionsMenu(item)),
  );
}

/** The Modified cell: "5 minutes ago", with the exact time in its title and for screen readers. */
function modifiedCell(item) {
  if (item.kind !== 'document') return { nodes: [h('span', { class: 'none', 'aria-hidden': 'true' }, '—')], short: '' };
  if (!item.revised_at) return { nodes: [h('span', { class: 'meta' }, 'Not saved yet')], short: 'Not saved yet' };
  const exact = formatDate(item.revised_at);
  const words = relativeTime(item.revised_at);
  return {
    nodes: [
      h('time', { datetime: item.revised_at, title: exact, 'data-at': item.revised_at }, words),
      h('span', { class: 'visually-hidden' }, ` (${exact})`),
    ],
    short: words,
  };
}

/** The Sharing column's badges: own permissions, and the person's limited access. */
function sharingMarks(item) {
  return [
    item.access ? null : h('small', { class: 'mark' }, 'Limited access'),
    item.inherits || item.kind === 'library' ? null : h('small', { class: 'mark own' }, 'Own permissions'),
  ].filter(Boolean);
}

/** A row's Actions menu ("⋯"): Rename, Move…, Copy…, Delete and Access, as far as allowed. */
function actionsMenu(item) {
  const entries = rowMenu(item);
  if (entries.length === 0) return null;
  const button = h(
    'button',
    { type: 'button', class: 'plain menu-button dots', 'aria-label': `Actions for ${item.name}`, 'data-for': item.id },
    '⋯',
  );
  const choose = (label, handler, danger = false) => {
    const entry = h('button', { type: 'button', role: 'menuitem', class: danger ? 'danger' : null }, label);
    entry.addEventListener('click', () => {
      actionsFor = item.id;
      handler();
    });
    return h('li', { role: 'none' }, entry);
  };
  const menu = h(
    'ul',
    { class: 'menu end', id: `actions-${item.id}`, 'aria-label': `Actions for ${item.name}` },
    ...entries.map((entry) => {
      if (entry === 'rename') return choose('Rename', () => askRename(item));
      if (entry === 'move') return choose('Move…', () => askMove([item], 'move'));
      if (entry === 'copy') return choose('Copy…', () => askMove([item], 'copy'));
      if (entry === 'delete') return choose('Delete', () => askDelete([item]), true);
      return h('li', { role: 'none' }, h('a', { role: 'menuitem', href: `/access.html?id=${seg(item.id)}` }, 'Access'));
    }),
  );
  setupMenu(button, menu);
  return h('span', { class: 'menu-wrap' }, button, menu);
}

function note(text, isError) {
  error.classList.toggle('ok', !isError);
  showMessage(error, text);
}

// ---- creating ----

/** Fills the New menu with what may be made here; people who may not add see no New button. */
function setupNewMenu() {
  $('new-wrap').hidden = !canAdd;
  $('drop-hint').hidden = !canAdd;
  if (!canAdd) return;
  const entries = newMenu({ folders: library?.folders, editors: docEditors, templates: templates.length });
  $('new-menu').replaceChildren(
    ...entries.map(({ key, label }) => {
      const entry = h('button', { type: 'button', role: 'menuitem' }, label);
      entry.addEventListener('click', () => chooseNew(key));
      return h('li', { role: 'none' }, entry);
    }),
  );
  setupMenu($('new-button'), $('new-menu'));
}

/** What a New menu entry does: a file picker for uploads, otherwise the naming dialog. */
function chooseNew(key) {
  if (key === 'files') return $('pick-files').click();
  if (key === 'folder-upload') return $('pick-folder').click();
  askCreate(key);
}

const createDialog = $('create-dialog');
$('create-cancel').addEventListener('click', () => createDialog.close());

/** Opens the small dialog that names a new folder, document or copy of a template. */
function askCreate(key) {
  creating = key;
  const folder = key === 'folder';
  const template = key === 'template';
  const editor = key.startsWith('doc:') ? key.slice(4) : null;
  const extension = editor && Object.hasOwn(EDITORS, editor) ? EDITORS[editor].extension : null;
  const titles = { text: 'New text document', gridcraft: 'New spreadsheet', wordcraft: 'New document', solidcraft: 'New design' };
  $('create-title').textContent = folder ? 'New folder' : template ? 'New from a template' : (titles[editor] ?? 'New document');
  $('create-template-field').hidden = !template;
  $('create-name-label').textContent = folder ? 'Folder name' : 'Name (optional)';
  $('create-name').required = folder;
  $('create-name').value = '';
  $('create-hint').textContent = folder
    ? ''
    : extension
      ? `Leave it empty for a default name. The ending ${extension} is added for you.`
      : 'Leave it empty for a default name.';
  $('create-submit').textContent = folder ? 'Create folder' : template ? 'Create' : 'Create and open';
  showMessage($('create-error'), '');
  $('create-dialog').showModal();
  (template ? $('create-template') : $('create-name')).focus();
}

$('create-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  const key = creating;
  const name = $('create-name').value.trim();
  const say = (text) => showMessage($('create-error'), text);
  say('');
  note('', false);
  if (key === 'folder') {
    if (!name) return say('Give the folder a name.');
    const result = await api('POST', `/nodes/${seg(nodeId)}/children`, { kind: 'folder', name });
    if (!result.ok) return say(messageFor(result.error));
    createDialog.close();
    note(`Folder “${result.data.name}” was created.`, false);
    return reload();
  }
  if (key === 'template') {
    const template = templates.find((t) => t.id === $('create-template').value);
    if (!template) return say('Choose a template.');
    const body = { template: template.id };
    if (self.kind === 'folder') body.parent = nodeId;
    if (name) body.name = name;
    const result = await api('POST', `/libraries/${seg(libraryStep.id)}/from-template`, body);
    if (!result.ok) {
      return say(policyRefusalText(result.error, result.data, name || template.name) ?? messageFor(result.error));
    }
    createDialog.close();
    note(`“${result.data.node.name}” was made from the template ${template.name}.`, false);
    return reload();
  }
  const editor = key.slice(4);
  const extension = Object.hasOwn(EDITORS, editor) ? EDITORS[editor].extension : null;
  const body = { parent: nodeId, editor };
  if (extension) body.name = withExtension(name, extension);
  else if (name) body.name = name;
  const result = await api('POST', '/docs', body);
  if (result.ok) location.assign(pageFor('document', result.data.id, result.data.editor));
  else say(result.data?.message ?? messageFor(result.error));
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
    checkScans: true,
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

/** Drops a row. A kept session that is not going to be resumed is given back to the server. */
function forgetUpload(outcome, { release = true } = {}) {
  if (release && outcome.keep && outcome.sessionId) api('DELETE', `/uploads/${seg(outcome.sessionId)}`);
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
    forgetUpload(outcome, { release: false });
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
  if (!result.ok) {
    return showMessage($('rename-error'), policyRefusalText(result.error, result.data, name) ?? messageFor(result.error));
  }
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
