// A library or a folder: its breadcrumb, its contents a page at a time, and what a person may
// do with each item. The server decides who may do what; this page only offers what the roles
// it was told allow and shows the server's refusals as plain sentences.

import { EDITORS, newDocEditors, withExtension } from './editor-host.js';
import {
  api,
  breadcrumbs,
  bulkSummary,
  can,
  canPlaceHere,
  confirmDialog,
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
import { filesFromDrop, filesFromPicker } from './upload.js';
import { createUploads, mountUploadTray } from './upload-tray.js';
import { setupMenu } from './menu.js';
import { sidebar } from './workspace.js';
import { runLatestAction, showToast } from './toast.js';
import { buildUndo, isUndoKey, performUndo } from './undo.js';
import { addPaletteActions } from './palette.js';
import { createDetails } from './details.js';
import {
  ITEM_TYPE,
  canDrop,
  clipboardKey,
  cutText,
  dragList,
  isFileDrag,
  isItemDrag,
  mountFolderTree,
  parseCut,
} from './tree.js';
import { fileIcon, fileKind, nextSort, parseSort, relativeTime, sortDescription, sortEntries, splitName, typeLabel } from './files.js';
import { addShortcut, isMacPlatform } from './shortcuts.js';
import { extendSelection, moveIndex, selectionText, toggleSelection } from './selection.js';
import { settleAll, softly } from './skeleton.js';
import { createPhotoView } from './photo-view.js';
import { createPhotoViewer } from './photo-viewer.js';
import {
  shouldReload,
  sourceAccess,
  sourceControlsEnabled,
  sourceName,
  sourceStateText,
  sourceSummary,
  unavailableMarkFor,
} from './source-panel.js';

const BULK_MAX = 500;
const GRID_PAGE = 200; // the grid's page, the most the server answers at once

const PAGE = 50;
const $ = (id) => document.getElementById(id);
const error = $('error');
const MISSING = 'This folder does not exist, or you do not have access to it.';

const me = await requireMe();
setupTopbar(me, error);
// The folder shown. While uploads run, another folder of the same library is opened in this
// page (see switchTo), so these change; everything else about the library stays.
let nodeId = queryId(location.search);
if (!nodeId) fail(MISSING);
let here = await showCrumbs($('crumbs'), nodeId, { linkLast: false });
if (!here) {
  offerRequest(error, nodeId);
  fail(MISSING);
}
let self = here.path[here.path.length - 1];
if (self.kind === 'site' || self.kind === 'document') {
  location.replace(pageFor(self.kind, self.id));
  await new Promise(() => {});
}
// The library this place is in, with its kind and settings (a folder is judged by its library).
const libraryStep = here.path.find((step) => step.kind === 'library');
const libraryInfo = libraryStep ? await api('GET', `/libraries/${seg(libraryStep.id)}`) : null;
const library = libraryInfo?.ok ? libraryInfo.data : null;
let source = null;
let sourceAccessState = 'unknown';
if (libraryStep) {
  const sourceAnswer = await api('GET', `/libraries/${seg(libraryStep.id)}/source`);
  sourceAccessState = sourceAccess(sourceAnswer);
  if (sourceAccessState === 'connected') source = sourceAnswer.data;
  else if (sourceAccessState === 'unknown') showMessage(error, messageFor(sourceAnswer.error));
} else {
  sourceAccessState = 'none';
}
const sourceControls = () => sourceControlsEnabled(sourceAccessState);
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
$('accepts-text').textContent = accepts.join(' ');
$('drop-accepts').textContent = accepts.join(' ');
// The library-level links live in the More menu; an entry the person may not use is left out.
const showEntry = (id, shown) => {
  $(id).hidden = !shown;
  $(id).closest('li').hidden = !shown;
};
if (libraryStep) $('settings-link').href = `/library-settings.html?id=${seg(libraryStep.id)}`;
let canAdd = false;
showPlace();
await siteChrome(here.path[0].id, {
  currentId: libraryStep?.id,
  footer: true,
  hero: libraryStep ? { library: libraryStep.id, url: library?.hero_image?.url ?? null } : null,
});

$('bin-link').href = `/bin.html?site=${seg(here.path[0].id)}`;
const siteId = here.path[0].id;
setupMenu($('more-button'), $('more-menu'));

// Everything the code below reads while the page is drawn is declared here, before the first
// await: a `let` or `const` further down would still be unusable when `reload` draws the list.
let items = [];
let next = null;
const selected = new Set(); // ids of the checked items
let tray = null; // the upload tray, made just after the uploads it shows
let switching = 0; // the latest folder asked for by switchTo; older answers are dropped
let moving = []; // the items the move or copy dialog is for
let moveMode = 'move';
let trail = [];
let moveRequest = 0; // the latest listing asked for; older answers are dropped
let renaming = null;
const views = libraryViews(library?.kind);
let view = 'list'; // 'grid' or 'photos' only in an image library
let photoView = null; // the Album view, made when first shown (see showPhotos)
let photoViewer = null; // its full-screen viewer and slideshow
let moreWatcher = null; // loads the next page as the end of the Album view comes near
let watcher = null; // loads a tile's thumbnail when it scrolls into view
let templates = [];
let creating = null; // what the create dialog makes: 'folder', 'doc:<editor>' or 'template'
let actionsFor = null; // the item whose Actions menu was used last, so focus can go back to it
let sort = loadSort(); // the list's order, remembered per viewer
let tree = null; // the folder tree beside the table (see setupTree)
let dragging = null; // the items being dragged onto a folder: { items, from }
let dropAt = null; // the element marked as where a drag would land
let ghost = null; // the small "3 items" picture under the pointer while dragging
let expandTimer = 0; // opens a tree folder held under a drag
let cut = loadCut(); // items cut with Ctrl+X, waiting to be pasted in another folder (kept for the tab)
let anchor = null; // where Shift+Up or Shift+Down started extending the selection
let anchorBase = new Set(); // what was selected when it started
let inline = null; // the rename in place under way: { id, input }
let sourceTimer = null; // polls only while the source is connecting or syncing
const MAC = isMacPlatform();
const KEY_MOD = MAC ? { text: 'Command', aria: 'Meta' } : { text: 'Ctrl', aria: 'Control' };
const UNDO_KEYS = `${KEY_MOD.text}+Z`;
const moveDialog = $('move-dialog');
const renameDialog = $('rename-dialog');
const movingIds = () => moving.map((item) => item.id);
const mayRename = (item) => sourceControls() && itemActions(item).rename;
const mayMove = (item) => sourceControls() && itemActions(item).move;
const mayRemove = (item) => sourceControls() && itemActions(item).remove;

// The details pane beside the table (a sheet on narrow screens): preview, facts and access of
// the selected row, with the row actions. Only for the list; the grid of an image library has none.
const details = createDetails({
  pane: $('details-pane'),
  sheet: $('details-sheet'),
  status: $('details-status'),
  rows: $('file-rows'),
  page: $('lib-body').closest('.page'),
  entry: (id) => items.find((item) => item.id === id) ?? null,
  act: (key, item) => {
    actionsFor = item.id;
    if (key === 'rename') renameItem(item);
    else if (key === 'move') askMove([item], 'move');
    else if (key === 'delete') deleteItems([item]);
  },
  me,
  enabled: () => view === 'list',
  readOnly: () => !sourceControls(),
});

renderSource();
$('source-sync').addEventListener('click', () => {
  if (sourceAccessState === 'unknown') refreshSource();
  else updateSourceNow();
});

// The uploads, in the tray at the bottom-right: they run whichever folder is on screen.
const uploads = createUploads({
  site: siteId,
  release: (session) => api('DELETE', `/uploads/${seg(session)}`),
  onChange: (entry) => tray.changed(entry),
  onIdle: () => uploadsIdle(),
});

tray = mountUploadTray({ uploads, openFolder: (id) => switchTo(id), confirm: confirmDialog });

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
settleAll(); // what could not be drawn is put back as it was (skeleton.js)
setupTree();
markCrumbs();
// Upload files on a new site's Get started list arrives as #upload: the New menu opens on Upload
// files, so one more press opens the file picker (a page cannot open it on its own).
if (location.hash === '#upload') {
  history.replaceState(null, '', location.pathname + location.search);
  if (canAdd) {
    $('new-button').click();
    $('new-menu').querySelector('[data-key="files"]')?.focus();
  }
}

// ---- repository source ----

function showAccepts() {
  const hidden = Number(source?.hidden) > 0 ? Number(source.hidden) : 0;
  $('source-hidden').hidden = hidden === 0;
  $('source-hidden-summary').textContent = hidden === 1 ? '1 file hidden by file rules' : `${hidden} files hidden by file rules`;
  $('accepts-text').textContent = canAdd ? accepts.join(' ') : '';
  $('accepts').hidden = !(canAdd && accepts.length > 0) && hidden === 0;
}

function renderSource() {
  const banner = $('source-banner');
  clearTimeout(sourceTimer);
  sourceTimer = null;
  $('source-name').hidden = true;
  $('source-fix').hidden = true;
  banner.hidden = sourceAccessState === 'none';
  if (sourceAccessState === 'none') {
    showAccepts();
    return;
  }
  if (sourceAccessState === 'unknown') {
    banner.dataset.tone = 'attention';
    $('source-state').textContent = 'Read only';
    $('source-headline').textContent = 'Could not check whether this library is connected to GitHub.';
    $('source-detail').textContent = 'This library stays read-only until its source status is known.';
    $('source-sync').textContent = 'Retry';
    $('source-sync').hidden = false;
    $('source-sync').disabled = false;
    showAccepts();
    return;
  }
  const summary = sourceSummary(source);
  banner.dataset.tone = summary.tone;
  $('source-state').textContent = sourceStateText(source.state);
  $('source-headline').textContent = summary.headline;
  $('source-detail').textContent = summary.detail;
  // An S3 answer carries the bucket for owners only; its name is shown only when it is there.
  const s3Bucket = source.kind === 's3' && typeof source.bucket === 'string' && source.bucket !== '';
  $('source-name').textContent = s3Bucket ? sourceName(source) : '';
  $('source-name').hidden = !s3Bucket;
  $('source-fix').hidden = !(s3Bucket && source.state === 'not_connected');
  if (libraryStep) $('source-fix').href = `/library-settings.html?id=${seg(libraryStep.id)}`;
  $('source-sync').textContent = 'Update now';
  $('source-sync').hidden = !can(here.access, 'manage_site');
  $('source-sync').disabled = ['connecting', 'syncing'].includes(source.state);
  showAccepts();
  sourceTimer = ['connecting', 'syncing'].includes(source.state) ? setTimeout(refreshSource, 10_000) : null;
}

async function refreshSource() {
  if (!libraryStep) return;
  $('source-sync').disabled = true;
  const result = await api('GET', `/libraries/${seg(libraryStep.id)}/source`);
  const controlsWereEnabled = sourceControls();
  const previous = source;
  sourceAccessState = sourceAccess(result);
  source = sourceAccessState === 'connected' ? result.data : null;
  if (sourceAccessState === 'unknown') showMessage(error, messageFor(result.error));
  else showMessage(error, '');
  showPlace();
  renderSource();
  if (sourceAccessState === 'connected' && shouldReload(previous, source)) {
    await reload({ preserve: true });
  } else if (controlsWereEnabled !== sourceControls()) render();
  if (sourceAccessState === 'none') {
    await setupTemplates();
    setupNewMenu();
  }
}

async function updateSourceNow() {
  if (!libraryStep || sourceAccessState !== 'connected') return;
  $('source-sync').disabled = true;
  showMessage(error, '');
  const result = await api('POST', `/libraries/${seg(libraryStep.id)}/source/sync`);
  if (!result.ok) {
    $('source-sync').disabled = false;
    showMessage(error, messageFor(result.error));
    return;
  }
  source = { ...source, state: 'syncing' };
  renderSource();
}

// ---- the listing ----

async function reload({ preserve = false } = {}) {
  const keptSelection = preserve ? new Set(selected) : new Set();
  const active = preserve ? document.activeElement : null;
  const focusedId = active?.closest?.('[data-id]')?.dataset.id ?? null;
  const focusedAction = active?.matches?.('button.menu-button') ?? false;
  clearList();
  for (const id of keptSelection) selected.add(id);
  await loadPage();
  if (preserve) {
    const visible = new Set(items.map((item) => item.id));
    for (const id of selected) if (!visible.has(id)) selected.delete(id);
    showSelection();
    if (focusedId) {
      const row = [...document.querySelectorAll('#items [data-id], #file-rows [data-id]')].find(
        (element) => element.dataset.id === focusedId,
      );
      (focusedAction ? row?.querySelector('button.menu-button') : row)?.focus();
    }
  }
  tree?.refresh();
}

function clearList() {
  items = [];
  next = null;
  selected.clear();
  anchor = null;
  inline = null;
}

/** The query of a page of the listing after `after` (null for the first). */
function listQuery(after) {
  const grid = view === 'grid' || view === 'photos';
  return `limit=${grid ? GRID_PAGE : PAGE}${grid ? `&view=${view}` : ''}${after ? `&after=${encodeURIComponent(after)}` : ''}`;
}

async function loadPage() {
  takePage(await api('GET', `/nodes/${seg(nodeId)}/children?${listQuery(next)}`));
}

/** Adds the page of the listing `result` answered, or says why there is none. */
function takePage(result) {
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
  const photos = view === 'photos';
  const lostFocus =
    document.activeElement === document.body ||
    $('items').contains(document.activeElement) ||
    $('file-rows').contains(document.activeElement);
  // The grid of an image library is a list of tiles; the list is a table that sorts.
  $('items').classList.toggle('grid', grid);
  $('items').hidden = !grid;
  $('file-table').hidden = grid || photos || items.length === 0;
  $('photos').hidden = !photos || items.length === 0;
  if (photos) showPhotos();
  $('items').replaceChildren(...(grid ? items.map(tile) : []));
  $('file-rows').replaceChildren(...(grid || photos ? [] : sortEntries(items, sort.key, sort.direction).map(row)));
  showSort();
  showSelection();
  showCut();
  details.redraw();
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
    const kept = localStorage.getItem('sharecraft-library-view');
    if (views.includes(kept)) view = kept;
  } catch {
    // no storage: the list is shown
  }
  for (const [id, which] of [['view-list', 'list'], ['view-grid', 'grid'], ['view-photos', 'photos']]) {
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
  $('view-photos').setAttribute('aria-pressed', String(view === 'photos'));
}

// ---- the Album view of an image library, its viewer and slideshow (photo-view.js) ----

/** Draws the Album view of what is loaded: folders as links, the pictures, and a note of the rest. */
function showPhotos() {
  if (!photoView) setupPhotos();
  const folders = items.filter((item) => item.kind === 'folder' || item.kind === 'library');
  $('photo-folders').hidden = folders.length === 0;
  $('photo-folders').replaceChildren(
    ...folders.map((item) => h('li', {}, h('a', { href: pageFor(item.kind, item.id), class: 'photo-folder' }, item.name))),
  );
  photoView.show(items);
  const shown = photoView.photos().length;
  const others = items.filter((item) => item.kind === 'document').length - shown;
  $('photo-others').hidden = others === 0;
  $('photo-others').textContent =
    others === 1
      ? 'One other file has no picture to show here; the List shows it.'
      : `${others} other files have no picture to show here; the List shows them.`;
  $('photo-slideshow').disabled = shown === 0;
}

/** Makes the Album view and its viewer, the first time they are shown. */
function setupPhotos() {
  photoViewer = createPhotoViewer({
    onClose: (item) => {
      if (item && view === 'photos') photoView.focusPhoto(item.id);
    },
  });
  photoView = createPhotoView({
    root: $('photos'),
    isSelected: (id) => selected.has(id),
    toggle: (id) => {
      anchor = null;
      setSelection(toggleSelection(selected, id));
    },
    open: (list, index) => photoViewer.open(list, index),
  });
  let grouped = true;
  try {
    grouped = localStorage.getItem('sharecraft-photos-grouped') !== 'false';
  } catch {
    // not remembered: by date
  }
  $('photo-group').setAttribute('aria-pressed', String(grouped));
  photoView.setGrouped(grouped);
  $('photo-group').addEventListener('click', () => {
    const on = $('photo-group').getAttribute('aria-pressed') !== 'true';
    $('photo-group').setAttribute('aria-pressed', String(on));
    try {
      localStorage.setItem('sharecraft-photos-grouped', String(on));
    } catch {
      // not remembered
    }
    photoView.setGrouped(on);
  });
  $('photo-slideshow').addEventListener('click', () => {
    const list = photoView.photos();
    // From the picture last focused, else the first.
    const from = Math.max(0, list.findIndex((item) => item.id === photoView.current()));
    photoViewer.open(list, from, { play: true });
  });
  // More of a large library loads as its end comes near.
  if ('IntersectionObserver' in globalThis) {
    moreWatcher = new IntersectionObserver(
      (entries) => {
        if (view !== 'photos' || !next || $('more').disabled || !entries.some((entry) => entry.isIntersecting)) return;
        $('more').disabled = true;
        loadPage().finally(() => {
          $('more').disabled = false;
          // Asked again: the end may still be in view after this page.
          moreWatcher.unobserve($('more'));
          moreWatcher.observe($('more'));
        });
      },
      { rootMargin: '600px' },
    );
    moreWatcher.observe($('more'));
  }
}

/** A tile of the grid: the thumbnail (loaded when it comes into view), the name and its marks. */
/**
 * The small text marks under an item's name: the shared marks, then the listing's unavailable
 * reason in words (never colour alone). The unavailable mark has an id the name links describe.
 */
function markNodes(item) {
  const unavailable = unavailableMarkFor(item);
  return [
    ...itemMarks(item).map((mark) => h('small', { class: 'mark warn' }, mark)),
    ...(unavailable ? [h('small', { class: 'mark warn', id: `unavail-${item.id}` }, unavailable)] : []),
  ];
}

/** The description of a name link: the unavailable mark's text, when there is one. */
function describedBy(item) {
  return unavailableMarkFor(item) === '' ? {} : { 'aria-describedby': `unavail-${item.id}` };
}

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
    h('a', { href: pageFor(item.kind, item.id, item.editor), class: 'tile-link', ...describedBy(item) }, picture, h('span', { class: 'tile-name' }, item.name)),
    h('span', { class: 'tile-foot' }, box, ...markNodes(item)),
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
  for (const id of ['bulk-move', 'bulk-cut', 'bulk-copy', 'bulk-delete']) $(id).hidden = !sourceControls();
  $('bulk-move').disabled = state.count === 0 || !allowed.every((a) => a.move);
  $('bulk-cut').disabled = $('bulk-move').disabled;
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
  const tr = h(
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
          // One line with an ellipsis on screen; the whole name stays in the text and the tooltip.
          h('a', { href: pageFor(item.kind, item.id, item.editor), class: 'name-link', title: item.name, ...describedBy(item) }, item.name),
          ...markNodes(item),
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
  details.row(tr, item);
  // A row the person may move can be dragged onto a folder (the link inside is not dragged on its own).
  // (Not with a finger: there a held row opens its Actions sheet, shell.js.)
  if (mayMove(item) && !globalThis.matchMedia?.('(pointer: coarse)').matches) {
    tr.draggable = true;
    tr.querySelector('a.name-link').draggable = false;
  }
  return tr;
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

/** A row's Actions menu ("⋯"): Details, then Rename, Move…, Copy…, Delete and Access, as far as allowed. */
function actionsMenu(item) {
  const entries = [
    'details',
    ...rowMenu(item).filter((entry) => sourceControls() || !['rename', 'move', 'copy', 'delete'].includes(entry)),
  ];
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
    ...entries.flatMap((entry) => {
      if (entry === 'details') return choose('Details', () => details.show(item, { focus: true }));
      if (entry === 'rename') {
        const renameEntry = choose('Rename', () => renameItem(item));
        renameEntry.firstChild.setAttribute('aria-keyshortcuts', 'F2');
        return renameEntry;
      }
      if (entry === 'move') {
        const cutEntry = choose('Cut', () => cutItems([item]));
        cutEntry.firstChild.setAttribute('aria-keyshortcuts', `${KEY_MOD.aria}+X`);
        return [choose('Move to…', () => askMove([item], 'move')), cutEntry];
      }
      if (entry === 'copy') return choose('Copy…', () => askMove([item], 'copy'));
      if (entry === 'delete') return choose('Delete', () => deleteItems([item]), true);
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
  const entries = newMenu({ folders: library?.folders, editors: docEditors, templates: templates.length });
  $('new-menu').replaceChildren(
    ...entries.map(({ key, label }) => {
      const entry = h('button', { type: 'button', role: 'menuitem', 'data-key': key }, label);
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
    await reload();
    showToast(`Created the folder ${result.data.name}`);
    return;
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
    await reload();
    showToast(`Made ${result.data.node.name} from the template ${template.name}`);
    return;
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
// Files and folders dropped anywhere on the page go into this library or folder.
// Items dragged from the table carry their own type (see tree.js), so they never open the overlay.
const hasFiles = (event) => isFileDrag(event.dataTransfer?.types);
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

/** Queues `files` (from a drop or a picker) for the folder on screen, behind any still running. */
function startUploads(files) {
  if (files.length === 0) return;
  note('', false);
  uploads.add(files, { id: nodeId, name: self.name });
}

/** Every upload has stopped: the tray sums up and the folder on screen shows what arrived. */
function uploadsIdle() {
  tray.idle();
  const arrived = uploads.entries.filter((entry) => entry.status === 'done');
  if (arrived.some((entry) => entry.destination.id === nodeId)) reload();
  else if (arrived.length) tree?.refresh();
}

// Leaving the page would stop the uploads, so the browser asks first.
addEventListener('beforeunload', (event) => {
  if (uploads.active() === 0) return;
  event.preventDefault();
  event.returnValue = '';
});

// While uploads run, a folder of this library opens in this page, so that they carry on.
document.addEventListener('click', (event) => {
  if (uploads.active() === 0 || event.defaultPrevented) return;
  if (event.button !== 0 || event.ctrlKey || event.metaKey || event.shiftKey || event.altKey) return;
  const link = event.target.closest?.('a[href]');
  if (!link || link.target || link.hasAttribute('download')) return;
  const url = new URL(link.href, location.href);
  if (url.origin !== location.origin || url.pathname !== '/library.html') return;
  const id = queryId(url.search);
  if (!id) return;
  event.preventDefault();
  switchTo(id);
});
history.replaceState({ place: nodeId }, '');
addEventListener('popstate', () => {
  const id = queryId(location.search);
  if (id && id !== nodeId) switchTo(id, { push: false });
});

/**
 * Shows the folder `id` in this page when it is in the same library (anywhere else is opened
 * the usual way): its breadcrumb, title, what may be done there and its contents.
 */
async function switchTo(id, { push = true } = {}) {
  if (id === nodeId) {
    await reload();
    $('title').focus();
    return;
  }
  const ask = (switching += 1);
  const found = await api('GET', `/nodes/${seg(id)}`);
  if (ask !== switching) return;
  const path = found.ok ? found.data.path : null;
  const last = path?.at(-1);
  const inLibrary = last && ['library', 'folder'].includes(last.kind) && libraryStep && path.some((step) => step.id === libraryStep.id);
  if (!inLibrary) {
    location.assign(last ? pageFor(last.kind, last.id) : `/library.html?id=${seg(id)}`);
    return;
  }
  // The new folder's first page is read before anything changes, so that the swap is one soft
  // crossfade (a view transition, skeleton.js) from the old folder to the new one.
  const first = await api('GET', `/nodes/${seg(id)}/children?${listQuery(null)}`);
  if (ask !== switching) return;
  await softly(() => {
    if (push) history.pushState({ place: id }, '', pageFor(last.kind, id));
    nodeId = id;
    here = { path, access: found.data.access };
    self = last;
    $('crumbs').replaceChildren(breadcrumbs(path, { linkLast: false }));
    markCrumbs();
    note('', false);
    showPlace();
    tree?.show(id, path.slice(path.findIndex((step) => step.id === libraryStep.id)));
    clearList();
    takePage(first);
    tree?.refresh();
  });
  scrollTo(0, 0);
  $('title').focus();
}

/** The parts of the page that belong to the folder shown: title, role, links, New and drop. */
function showPlace() {
  $('title').textContent = self.name;
  document.title = `${self.name} — ShareCraft`;
  $('kind').textContent =
    (self.kind === 'library' ? libraryKindLabel(library?.kind) : 'Folder') +
    (here.access ? ` · ${roleLine(here.access)}` : ' · Limited access');
  showEntry('settings-link', can(here.access, 'manage_site') && libraryStep !== undefined);
  showEntry('access-link', can(here.access, 'manage_access'));
  showEntry('explorer-link', can(here.access, 'manage_access'));
  $('explorer-link').href = `/explorer.html?root=${seg(nodeId)}`;
  $('access-link').href = `/access.html?id=${seg(nodeId)}`;
  canAdd = can(here.access, 'add') && sourceControls();
  showAccepts();
  $('drop-target').textContent = self.name;
  $('new-wrap').hidden = !canAdd;
  $('drop-hint').hidden = !canAdd;
}

// ---- rename ----

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
  await reload();
  offerUndo(buildUndo({ op: 'rename', id: item.id, name: result.data.name }, { id: item.id, name: item.name }));
});

// ---- bulk: delete, move and copy ----

$('bulk-delete').addEventListener('click', () => deleteItems(selectedItems()));
$('bulk-move').addEventListener('click', () => askMove(selectedItems(), 'move'));
$('bulk-cut').addEventListener('click', () => cutItems(selectedItems()));
$('bulk-copy').addEventListener('click', () => askMove(selectedItems(), 'copy'));

/**
 * Deletes `list` to the recycle bin at once, with no question: the toast that follows offers
 * Undo, which restores them from the bin. When a row's own menu was used, focus goes to the next row.
 */
async function deleteItems(list) {
  if (list.length === 0) return;
  if (list.length === 1 && actionsFor === list[0].id) {
    const order = sortEntries(items, sort.key, sort.direction);
    const at = order.findIndex((item) => item.id === list[0].id);
    actionsFor = (order[at + 1] ?? order[at - 1])?.id ?? null;
  }
  await runBulk('delete', list, null);
}

/**
 * Sends `list` through `POST /api/bulk` (500 at a time) to `place` (a move or copy). What went
 * well is said in a toast (with Undo for a delete or a move); when several items were sent and
 * some failed, or names had to change, every item is listed under the bar.
 */
async function runBulk(op, list, place, from = nodeId) {
  note('', false);
  $('bulk-results-box').hidden = true;
  $('bulk-results').hidden = true;
  const results = [];
  for (let at = 0; at < list.length; at += BULK_MAX) {
    const batch = list.slice(at, at + BULK_MAX);
    const body = { op, items: batch.map((item) => item.id) };
    if (place) body.parent = place.id;
    const result = await api('POST', '/bulk', body);
    if (!result.ok) {
      for (const item of batch) results.push({ id: item.id, ok: false, error: result.error });
    } else results.push(...result.data);
  }
  const names = Object.fromEntries(list.map((item) => [item.id, item.name]));
  const summary = bulkSummary(op, results, names);
  const done = results.filter((result) => result.ok).map((result) => result.id);
  const listed = (summary.failed > 0 && list.length > 1) || results.some((result) => result.ok && result.renamed);
  if (listed) {
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
  }
  await reload();
  if (summary.failed > 0 && list.length === 1) {
    showToast(`Could not ${op} ${list[0].name}. ${summary.lines[0].text}`, { kind: 'error' });
    return;
  }
  if (done.length === 0) return;
  if (op === 'copy') {
    const what = done.length === 1 ? names[done[0]] : `${done.length} items`;
    showToast(`Copied ${what} to ${place.name}`);
    return;
  }
  offerUndo(
    buildUndo(
      { op, done, to: place },
      list.map((item) => ({ id: item.id, name: item.name, parent: from })),
    ),
  );
}

// ---- undo: a toast's Undo button, or Ctrl+Z ----

/** Says what was done, with an Undo button that reverses it and then says how that went. */
function offerUndo(undo) {
  if (!undo) return;
  showToast(undo.message, {
    announce: `${undo.message}. To undo, press ${UNDO_KEYS}.`,
    action: {
      label: undo.label,
      run: async () => {
        const outcome = await performUndo(undo, api);
        // The list of what a bulk action did is out of date once it is undone.
        $('bulk-results-box').hidden = true;
        $('bulk-results').hidden = true;
        await reload();
        showToast(outcome.message, { kind: outcome.ok ? 'ok' : 'error' });
      },
    },
  });
}

// Ctrl+Z (Cmd+Z) outside text fields and dialogs undoes the newest action still on screen.
addShortcut({
  keys: ['Mod+Z'],
  label: 'Undo the last delete, move or rename',
  scope: 'library',
  when: (event) => isUndoKey(event),
  run: () => runLatestAction(),
});

// ---- move and copy (one dialog) ----

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
  await runBulk(mode, list, place);
});

// ---- the folder tree ----

/**
 * The library's folder tree, in the page's one sidebar beneath the site's links (see
 * workspace.js): a column that folds to a rail from 1000 pixels, a drawer opened from the
 * Folders (or Menu) button below that.
 */
function setupTree() {
  const page = $('lib-body').closest('.page');
  const side = sidebar(page);
  side.opener($('folders-toggle'));
  if (!libraryStep || library?.folders === false) {
    $('folder-pane').remove();
    if (side.state.side === 'none') side.dissolve();
    return;
  }
  const pane = $('folder-pane');
  pane.hidden = false;
  side.folders(pane);
  const steps = here.path.slice(here.path.indexOf(libraryStep));
  tree = mountFolderTree({
    list: $('folder-tree'),
    root: { id: libraryStep.id, name: libraryStep.name, kind: 'library', access: libraryStep.access ?? null },
    path: steps,
    currentId: nodeId,
    onDraw: () => {
      dropAt = null;
    },
    // While uploads run, a folder chosen in the tree opens in this page.
    onOpen: (node) => {
      if (uploads.active() === 0) return false;
      switchTo(node.id);
      return true;
    },
  });
  tree.reveal();
}

/** Each breadcrumb step knows its place in the path, so that items can be dropped on it. */
function markCrumbs() {
  $('crumbs')
    .querySelectorAll('li')
    .forEach((li, at) => {
      li.dataset.at = String(at);
    });
}

// ---- dragging items onto a folder ----

/**
 * Where a drag over `element` would land: `{ mark, target, ancestry }` for a tree folder, a
 * folder row of the table or a breadcrumb step; null elsewhere (files are not places).
 */
function dropTargetOf(element) {
  const treeItem = element?.closest?.('#folder-tree li[role=treeitem]');
  if (treeItem) {
    const node = tree?.node(treeItem.dataset.id);
    if (!node) return null;
    return { mark: treeItem.querySelector(':scope > .tree-row'), target: node, ancestry: tree.ancestry(node.id) };
  }
  const tr = element?.closest?.('#file-rows tr[data-id]');
  if (tr) {
    const item = items.find((entry) => entry.id === tr.dataset.id);
    if (!item || !['folder', 'library'].includes(item.kind)) return null;
    return { mark: tr, target: item, ancestry: [...here.path.map((step) => step.id), item.id] };
  }
  const crumb = element?.closest?.('#crumbs li[data-at]');
  if (crumb) {
    const at = Number(crumb.dataset.at);
    const step = here.path[at];
    if (!step) return null;
    return { mark: crumb.firstElementChild, target: step, ancestry: here.path.slice(0, at + 1).map((s) => s.id) };
  }
  return null;
}

function draggedWhat(list) {
  return list.length === 1 ? list[0].name : `${list.length} items`;
}

$('file-rows').addEventListener('dragstart', (event) => {
  const tr = event.target.closest?.('tr[data-id]');
  const item = tr && items.find((entry) => entry.id === tr.dataset.id);
  if (!item || view !== 'list' || !event.dataTransfer) return;
  const list = dragList(item.id, selected, items).map((entry) => ({
    id: entry.id,
    name: entry.name,
    kind: entry.kind,
    movable: mayMove(entry),
  }));
  dragging = { items: list, from: nodeId };
  event.dataTransfer.effectAllowed = 'move';
  event.dataTransfer.setData(ITEM_TYPE, JSON.stringify(list.map((entry) => entry.id)));
  event.dataTransfer.setData('text/plain', list.map((entry) => entry.name).join('\n'));
  ghost = h('div', { class: 'drag-ghost', 'aria-hidden': 'true' }, `Move ${draggedWhat(list)}`);
  document.body.append(ghost);
  event.dataTransfer.setDragImage?.(ghost, 14, 14);
  for (const row of $('file-rows').querySelectorAll('tr[data-id]')) {
    row.classList.toggle('drag-source', list.some((entry) => entry.id === row.dataset.id));
  }
  dragHint(`Drop on a folder to move ${draggedWhat(list)}.`, true);
});

document.addEventListener('dragover', (event) => {
  if (!dragging || !isItemDrag(event.dataTransfer?.types)) return;
  const found = dropTargetOf(event.target);
  const verdict = found ? canDrop(dragging.items, found.target, found.ancestry, { from: dragging.from }) : null;
  markDrop(found, verdict);
  if (verdict?.ok) {
    event.preventDefault();
    event.dataTransfer.dropEffect = 'move';
  } else event.dataTransfer.dropEffect = 'none';
});

document.addEventListener('drop', async (event) => {
  if (!dragging || !isItemDrag(event.dataTransfer?.types)) return;
  event.preventDefault();
  const drag = dragging;
  const found = dropTargetOf(event.target);
  endDrag();
  if (!found) return;
  const verdict = canDrop(drag.items, found.target, found.ancestry, { from: drag.from });
  if (!verdict.ok) {
    showToast(verdict.reason, { kind: 'error' });
    return;
  }
  await runBulk('move', drag.items, { id: found.target.id, name: found.target.name }, drag.from);
});

$('file-rows').addEventListener('dragend', () => endDrag());

/** Marks where the drag would land (or why it cannot), and opens a closed tree folder held under it. */
function markDrop(found, verdict) {
  const mark = found?.mark ?? null;
  if (mark === dropAt) return;
  dropAt?.classList.remove('drop-ok', 'drop-no');
  dropAt = mark;
  clearTimeout(expandTimer);
  if (!mark) {
    dragHint(`Drop on a folder to move ${draggedWhat(dragging.items)}.`, true);
    return;
  }
  mark.classList.add(verdict.ok ? 'drop-ok' : 'drop-no');
  dragHint(verdict.ok ? verdict.message : verdict.reason, verdict.ok);
  const treeItem = mark.closest('#folder-tree li[role=treeitem]');
  if (treeItem?.getAttribute('aria-expanded') === 'false') {
    expandTimer = setTimeout(() => {
      if (dragging && dropAt === mark) tree?.expand(treeItem.dataset.id);
    }, 800);
  }
}

function dragHint(text, ok) {
  $('drag-status').textContent = text;
  $('drag-status').classList.toggle('no', !ok);
}

function endDrag() {
  dragging = null;
  clearTimeout(expandTimer);
  dropAt?.classList.remove('drop-ok', 'drop-no');
  dropAt = null;
  ghost?.remove();
  ghost = null;
  dragHint('', true);
  for (const row of $('file-rows').querySelectorAll('tr.drag-source')) row.classList.remove('drag-source');
}

// ---- cut and paste: Ctrl+X on selected rows, Ctrl+V in another folder ----

function loadCut() {
  try {
    return parseCut(sessionStorage.getItem('sharecraft-cut'));
  } catch {
    return null; // no storage: nothing was cut
  }
}

function saveCut() {
  try {
    if (cut) sessionStorage.setItem('sharecraft-cut', JSON.stringify(cut));
    else sessionStorage.removeItem('sharecraft-cut');
  } catch {
    // kept on this page only
  }
}

/** Whether the cut items may be pasted here, as `canDrop` answers. */
function pasteVerdict() {
  if (!sourceControls()) return { ok: false, reason: 'This library cannot be changed here.' };
  const place = { id: nodeId, name: self.name, kind: self.kind, access: here.access };
  return canDrop(cut.items, place, here.path.map((step) => step.id), { from: cut.from });
}

/** Cuts the items of `list` that the person may move; they are moved when pasted in another folder. */
function cutItems(list) {
  const movable = list.filter(mayMove);
  if (movable.length === 0) {
    showToast(list.length === 1 ? `You cannot move ${list[0].name}.` : 'You cannot move these items.', { kind: 'error' });
    return;
  }
  cut = { from: nodeId, fromName: self.name, items: movable.map(({ id, name }) => ({ id, name })) };
  saveCut();
  showCut();
  const left = list.length - movable.length;
  $('cut-status').textContent =
    `${cutText(cut.items)}.${left ? ` ${left} you cannot move ${left === 1 ? 'was' : 'were'} left out.` : ''} ` +
    `Open the folder to move to and press ${KEY_MOD.text}+V, or press Escape to cancel.`;
}

/** The bar that says what is cut, with Paste here and Cancel; the cut rows are dimmed. */
function showCut() {
  $('cut-bar').hidden = !cut || !sourceControls();
  const ids = new Set(cut && cut.from === nodeId ? cut.items.map((item) => item.id) : []);
  for (const tr of $('file-rows').querySelectorAll('tr[data-id]')) tr.classList.toggle('cut', ids.has(tr.dataset.id));
  if (!cut) return;
  $('cut-text').textContent = cutText(cut.items);
  const verdict = pasteVerdict();
  $('cut-paste').disabled = !verdict.ok;
  $('cut-paste').textContent = `Paste here (${KEY_MOD.text}+V)`;
  $('cut-hint').hidden = verdict.ok;
  $('cut-hint').textContent = verdict.ok
    ? ''
    : cut.from === nodeId
      ? 'Open the folder to move to, then paste.'
      : verdict.reason;
}

async function pasteCut() {
  if (!cut) return;
  const verdict = pasteVerdict();
  if (!verdict.ok) {
    showToast(cut.from === nodeId ? `${verdict.reason} Open another folder to paste.` : verdict.reason, { kind: 'error' });
    return;
  }
  const { items: list, from } = cut;
  const back = $('cut-bar').contains(document.activeElement);
  cut = null;
  saveCut();
  showCut();
  await runBulk('move', list, { id: nodeId, name: self.name }, from);
  if (back) $('file-rows').querySelector('tr[tabindex="0"]')?.focus();
}

function cancelCut() {
  if (!cut) return;
  const back = $('cut-bar').contains(document.activeElement);
  cut = null;
  saveCut();
  showCut();
  $('cut-status').textContent = 'Nothing is waiting to be moved now.';
  if (back) $('file-rows').querySelector('tr[tabindex="0"]')?.focus();
}

$('cut-paste').addEventListener('click', () => pasteCut());
$('cut-cancel').addEventListener('click', () => cancelCut());
$('bulk-cut').setAttribute('aria-keyshortcuts', `${KEY_MOD.aria}+X`);
$('cut-paste').setAttribute('aria-keyshortcuts', `${KEY_MOD.aria}+V`);

/** What Ctrl+X cuts: the selected items, else the focused row's. */
function cutList() {
  const focused = document.activeElement?.closest?.('#file-rows tr[data-id]');
  if (selected.size) return selectedItems();
  return focused ? items.filter((item) => item.id === focused.dataset.id) : [];
}

addShortcut({
  keys: ['Mod+X'],
  label: 'Cut the selected items, to move them',
  scope: 'library',
  shown: () => sourceControls(),
  when: (event) => sourceControls() && clipboardKey(event) === 'cut' && cutList().length > 0,
  run: () => cutItems(cutList()),
});
addShortcut({
  keys: ['Mod+V'],
  label: 'Paste the cut items into this folder',
  scope: 'library',
  shown: () => sourceControls(),
  when: (event) => sourceControls() && clipboardKey(event) === 'paste' && Boolean(cut),
  run: () => pasteCut(),
});
addShortcut({
  keys: ['Escape'],
  label: 'Cancel a cut',
  scope: 'library',
  shown: () => Boolean(cut),
  when: (event, context) => !context.dialog && clipboardKey(event) === 'cancel' && Boolean(cut) && !dragging,
  run: () => cancelCut(),
});

// ---- the keyboard: the current row, selection, rename in place and the rest ----

// Declarations, not consts: a row's menu can call these while the page is still loading.
function fileRows() {
  return [...$('file-rows').querySelectorAll('tr[data-id]')];
}
function rowFor(id) {
  return fileRows().find((tr) => tr.dataset.id === id) ?? null;
}
function itemFor(id) {
  return items.find((item) => item.id === id) ?? null;
}
function choosable(id) {
  return itemFor(id)?.kind !== 'library';
}

/** The row a key press came from (not its Actions menu), or null. */
function rowOf(event) {
  const tr = event.target?.closest?.('#file-rows tr[data-id]');
  return tr && !event.target.closest('.menu-wrap') && !event.target.closest('input.rename-inline') ? tr : null;
}

/** Whether keys for the files are meant here: focus in the table or grid, or on nothing in particular. */
function inFiles(event) {
  const target = event.target;
  return target === document.body || Boolean(target?.closest?.('#file-table, #items, #photos, #bulk-bar, .details-pane, .details-sheet'));
}

/** The item of the focused row, else the current row's. */
function currentItem() {
  const tr = document.activeElement?.closest?.('#file-rows tr[data-id]');
  return itemFor(tr?.dataset.id ?? details.current());
}

/** Makes `tr` the current row and focuses it; the details follow after a short pause. */
function goToRow(tr, delay = 250) {
  if (!tr) return;
  tr.focus();
  const item = itemFor(tr.dataset.id);
  if (item) details.select(item, delay);
}

/** Replaces the selection, updates the boxes and the bar, and says how many are selected. */
function setSelection(next) {
  selected.clear();
  for (const id of next) selected.add(id);
  if (view === 'grid') render();
  else if (view === 'photos') {
    photoView?.refreshSelection();
    showSelection();
  } else {
    for (const tr of fileRows()) {
      const box = tr.querySelector('td.c-select input');
      if (box) box.checked = selected.has(tr.dataset.id);
    }
    showSelection();
  }
  // The bar's count says "3 selected" by itself; an empty selection leaves it empty.
  $('keys-status').textContent = selected.size === 0 ? selectionText(0) : '';
}

/** Up, Down, Home, End, Page Up and Page Down; with Shift the selection follows from the anchor. */
function moveRow(event, extend) {
  const trs = fileRows();
  const tr = rowOf(event);
  const at = trs.indexOf(tr);
  const height = tr?.offsetHeight || 44;
  const page = Math.max(1, Math.floor(innerHeight / height) - 2);
  const to = moveIndex(event.key, at, trs.length, page);
  if (to === null) return false;
  const target = trs[to];
  if (extend) {
    if (!anchor || !rowFor(anchor)) {
      anchor = tr.dataset.id;
      anchorBase = new Set(selected);
    }
    setSelection(extendSelection(trs.map((row) => row.dataset.id), anchorBase, anchor, target.dataset.id, choosable));
  } else anchor = null;
  goToRow(target);
  target.scrollIntoView?.({ block: 'nearest' });
}

function listShown() {
  return view === 'list' && items.length > 0;
}

addShortcut({
  keys: ['ArrowDown', 'ArrowUp'],
  label: 'Move to the next or previous row',
  scope: 'library',
  shown: listShown,
  when: (event) => listShown() && Boolean(rowOf(event)),
  run: (event) => moveRow(event, false),
});
addShortcut({
  keys: ['Home', 'End'],
  label: 'Move to the first or last row',
  scope: 'library',
  shown: listShown,
  when: (event) => listShown() && rowOf(event) === event.target,
  run: (event) => moveRow(event, false),
});
addShortcut({
  keys: ['PageDown', 'PageUp'],
  label: 'Move a screen down or up',
  scope: 'library',
  shown: listShown,
  when: (event) => listShown() && Boolean(rowOf(event)),
  run: (event) => moveRow(event, false),
});
addShortcut({
  keys: ['Shift+ArrowDown', 'Shift+ArrowUp'],
  label: 'Select the rows you move over',
  scope: 'library',
  shown: listShown,
  when: (event) => listShown() && Boolean(rowOf(event)),
  run: (event) => moveRow(event, true),
});
addShortcut({
  keys: ['Space'],
  label: 'Select or unselect the current row',
  scope: 'library',
  shown: listShown,
  when: (event) => listShown() && rowOf(event) === event.target && choosable(event.target.dataset.id),
  run: (event) => {
    anchor = null;
    setSelection(toggleSelection(selected, event.target.dataset.id));
  },
});
addShortcut({
  keys: ['Mod+A'],
  label: 'Select everything shown',
  scope: 'library',
  shown: () => items.length > 0,
  when: (event) => items.some((item) => item.kind !== 'library') && inFiles(event),
  run: () => {
    anchor = null;
    setSelection(items.filter((item) => item.kind !== 'library').map((item) => item.id));
  },
});
addShortcut({
  keys: ['Enter'],
  label: 'Open the current row',
  scope: 'library',
  shown: listShown,
  when: (event) => listShown() && rowOf(event) === event.target,
  // Through its link, so that a folder opens in this page while uploads run.
  run: (event) => event.target.querySelector('a.name-link')?.click(),
});
addShortcut({
  keys: ['Backspace', 'Alt+ArrowUp'],
  label: 'Go up to the folder above',
  scope: 'library',
  shown: () => here.path.length > 1,
  when: () => here.path.length > 1,
  run: () => goUp(),
});
addShortcut({
  keys: ['F2'],
  label: 'Rename the current row',
  scope: 'library',
  shown: () => sourceControls() && listShown(),
  when: (event) => sourceControls() && listShown() && inFiles(event) && Boolean(currentItem()),
  run: () => {
    const item = currentItem();
    if (!mayRename(item)) {
      showToast(`You cannot rename ${item.name}.`, { kind: 'error' });
      return;
    }
    actionsFor = null;
    renameInPlace(item, { back: 'row' });
  },
});
addShortcut({
  keys: ['Delete'],
  label: 'Move the selected items to the recycle bin (with Undo)',
  scope: 'library',
  shown: () => sourceControls() && items.length > 0,
  when: (event) => sourceControls() && inFiles(event) && (selected.size > 0 || (view === 'list' && Boolean(currentItem()))),
  run: () => keyDelete(),
});
addShortcut({
  keys: ['n'],
  label: 'New folder',
  scope: 'library',
  shown: () => canAdd && library?.folders !== false,
  when: () => canAdd && library?.folders !== false,
  run: () => askCreate('folder'),
});
addShortcut({
  keys: ['u'],
  label: 'Upload files',
  scope: 'library',
  shown: () => canAdd,
  when: () => canAdd,
  run: () => $('pick-files').click(),
});

/** The folder above (or the site, above a library): in this page while uploads run, if it can. */
function goUp() {
  const parent = here.path[here.path.length - 2];
  if (!parent) return;
  if (uploads.active() > 0 && ['library', 'folder'].includes(parent.kind)) switchTo(parent.id);
  else location.assign(pageFor(parent.kind, parent.id));
}

/** Delete from the keyboard: the selection, else the current row; focus goes to the row after. */
async function keyDelete() {
  const list = selected.size ? selectedItems() : [currentItem()].filter(Boolean);
  if (list.length === 0) return;
  const refused = list.find((item) => !mayRemove(item));
  if (refused) {
    showToast(list.length === 1 ? `You cannot delete ${refused.name}.` : `You cannot delete ${refused.name}, so nothing was deleted.`, { kind: 'error' });
    return;
  }
  const gone = new Set(list.map((item) => item.id));
  const order = fileRows().map((tr) => tr.dataset.id);
  const from = order.indexOf(currentItem()?.id ?? list[0].id);
  const after = order.slice(from + 1).find((id) => !gone.has(id)) ?? order.slice(0, Math.max(0, from)).reverse().find((id) => !gone.has(id)) ?? null;
  const hadFocus = $('file-rows').contains(document.activeElement);
  actionsFor = null;
  await deleteItems(list);
  const tr = after ? rowFor(after) : null;
  if (tr && hadFocus) goToRow(tr, 0);
}

// ---- rename in place: the name cell becomes a field ----

/** Rename from a menu: in place in the list, in the dialog in the grid. */
function renameItem(item) {
  if (view === 'list' && rowFor(item.id)) renameInPlace(item, { back: 'actions' });
  else askRename(item);
}

/**
 * Turns the name of `item`'s row into a field with the name selected up to its ending (as
 * desktop file managers do). Enter saves it through the same request as the dialog, with the
 * Undo toast; Escape, or leaving it unchanged, keeps the old name. Focus then goes back to the
 * row (`back: 'row'`) or to its Actions button.
 */
function renameInPlace(item, { back = 'row' } = {}) {
  const tr = rowFor(item.id);
  const link = tr?.querySelector('a.name-link');
  if (!link) return askRename(item);
  if (inline?.input.isConnected) inline.input.blur();
  const input = h('input', {
    type: 'text',
    class: 'rename-inline',
    value: item.name,
    maxlength: '255',
    autocomplete: 'off',
    spellcheck: 'false',
    'aria-label': `New name for ${item.name}`,
    'aria-describedby': 'rename-help',
  });
  const draggable = tr.draggable;
  tr.draggable = false;
  link.hidden = true;
  link.after(input);
  inline = { id: item.id, input };
  let busy = false;
  let finished = false;

  const close = () => {
    finished = true;
    if (inline?.input === input) inline = null;
    input.remove();
    link.hidden = false;
    tr.draggable = draggable;
  };
  const focusBack = () => {
    const row = rowFor(item.id);
    if (back === 'actions') row?.querySelector('button.menu-button')?.focus();
    else if (row) goToRow(row, 0);
  };
  const finish = async (save, { left = false } = {}) => {
    if (finished || busy) return;
    const name = input.value.trim();
    if (!save || name === item.name) {
      const hadFocus = document.activeElement === input || document.activeElement === document.body;
      close();
      if (hadFocus) focusBack();
      $('keys-status').textContent = 'Name not changed.';
      return;
    }
    if (!name) {
      if (left) return close(); // left empty: the old name stays
      showToast('Give it a name, or press Escape to keep the old one.', { kind: 'error' });
      input.focus();
      return;
    }
    busy = true;
    input.setAttribute('aria-busy', 'true');
    input.readOnly = true;
    const result = await api('PATCH', `/nodes/${seg(item.id)}`, { name });
    busy = false;
    input.removeAttribute('aria-busy');
    input.readOnly = false;
    if (!result.ok) {
      showToast(policyRefusalText(result.error, result.data, name) ?? messageFor(result.error), { kind: 'error' });
      // Focus that went elsewhere stays there, and the old name with it; otherwise try again.
      if (left || document.activeElement !== input) close();
      else if (input.isConnected) {
        input.focus();
        input.select();
      }
      return;
    }
    close();
    if (back === 'row') actionsFor = null;
    await reload();
    focusBack();
    offerUndo(buildUndo({ op: 'rename', id: item.id, name: result.data.name }, { id: item.id, name: item.name }));
  };

  input.addEventListener('keydown', (event) => {
    if (event.key === 'Enter') {
      event.preventDefault();
      finish(true);
    } else if (event.key === 'Escape') {
      event.preventDefault();
      event.stopPropagation();
      finish(false);
    }
  });
  // Leaving the field saves, as on the desktop (unless the row was drawn again meanwhile).
  input.addEventListener('blur', () => {
    if (input.isConnected) finish(true, { left: true });
  });
  // A click in the field is not a click on the row.
  input.addEventListener('click', (event) => event.stopPropagation());
  input.focus();
  input.setSelectionRange(0, splitName(item.name, { folder: item.kind !== 'document' }).base.length);
}

function fail(text) {
  settleAll();
  showMessage(error, text);
  throw new Error(text);
}
