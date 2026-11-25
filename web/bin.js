// The recycle bin of a site: what was deleted (the first stage, for whoever may restore it) and,
// for people who manage the site, the second stage. The server decides who sees and does what;
// this page lists, groups the entries by when they were deleted, finds them by name or place,
// restores or removes one or many (with a toast, and Undo after a restore), asks to confirm
// before anything is erased, and shows the answers in words. The list logic is in recycle.js.

import {
  api,
  can,
  confirmDialog,
  daysLeftText,
  emptyBinText,
  formatDate,
  formatSize,
  h,
  messageFor,
  offerRequest,
  pageFor,
  queryId,
  regionalSettings,
  requireMe,
  seg,
  setupTopbar,
  showCrumbs,
  showMessage,
  siteChrome,
} from './common.js';
import { fileIcon, relativeTime } from './files.js';
import { sourceWriteErrorText } from './source-panel.js';
import {
  BIN_DAYS,
  binKind,
  binPathText,
  binSummary,
  daysLeftLevel,
  emptyText,
  eraseText,
  filterBin,
  filtering,
  groupByDeletedDate,
  kindChoices,
  restoredNote,
  restoredText,
  shownText,
} from './recycle.js';
import { extendSelection, moveIndex, selectionText, toggleSelection } from './selection.js';
import { addShortcut, isMacPlatform } from './shortcuts.js';
import { runLatestAction, showToast } from './toast.js';
import { buildUndo, isUndoKey, performUndo } from './undo.js';
import { settle, settleAll } from './skeleton.js';

const PAGE = 500; // entries asked for at a time
const LOAD = 2000; // entries loaded before "Show more" asks for the rest
const $ = (id) => document.getElementById(id);
const error = $('error');
const MISSING = 'This site does not exist, or you do not have access to it.';
const UNDO_KEYS = `${isMacPlatform() ? 'Command' : 'Ctrl'}+Z`;

// State the handlers below use. Declared before the start-up code that awaits.
let stage = location.hash === '#second' ? 'second' : 'first';
let entries = [];
let next = null;
let loading = 0; // the latest listing asked for; older answers are dropped
let shown = []; // the entries the search and filters keep, in the order drawn
let selected = new Set();
let current = null; // the id of the current row: the one row in the tab order
let anchor = null; // where Shift+Up or Shift+Down started extending the selection
let anchorBase = new Set();
let busy = false; // a restore or delete is under way
const filters = { text: '', by: 'anyone', kind: '' };
const places = new Map(); // path key -> Promise of { href } or null: where a path's folder is now
const childLists = new Map(); // node id -> Promise of its children (libraries and folders)
let me = null;
let siteId = null;
let shownTimer = 0; // says "Showing 3 of 14" once typing pauses

me = await requireMe();
setupTopbar(me, error);
siteId = queryId(location.search, 'site');
if (!siteId) fail(MISSING);
const here = await showCrumbs($('crumbs'), siteId, { linkLast: true });
if (!here) {
  offerRequest(error, siteId);
  fail(MISSING);
}
await siteChrome(siteId);
const manageSite = can(here.access, 'manage_site');
document.title = `Recycle bin — ${here.path[here.path.length - 1].name} — ShareCraft`;
$('stages').hidden = !manageSite;
if (!manageSite) stage = 'first';

$('stage-first').addEventListener('click', (event) => choose(event, 'first'));
$('stage-second').addEventListener('click', (event) => choose(event, 'second'));
$('more').addEventListener('click', () => loadMore());
$('empty-bin').addEventListener('click', emptyBin);
$('bulk-restore').addEventListener('click', () => restoreEntries(selectedEntries()));
$('bulk-delete').addEventListener('click', () => removeEntries(selectedEntries()));
$('select-all').addEventListener('change', (event) => {
  anchor = null;
  setSelection(event.target.checked ? shown.map((entry) => entry.id) : []);
});
$('bin-search').addEventListener('input', (event) => {
  filters.text = event.target.value;
  applyFilters();
});
$('bin-by').addEventListener('change', (event) => {
  filters.by = event.target.value;
  applyFilters();
});
$('bin-kind').addEventListener('change', (event) => {
  filters.kind = event.target.value;
  applyFilters();
});
$('clear-filters').addEventListener('click', () => {
  filters.text = '';
  filters.by = 'anyone';
  filters.kind = '';
  $('bin-search').value = '';
  $('bin-by').value = 'anyone';
  $('bin-kind').value = '';
  applyFilters();
  $('bin-search').focus();
});
registerKeys();
await reload();
// The head and the bar are drawn: their heights are remembered (skeleton.js).
settle($('bin-head'));
settle($('bin-bar'));
settleAll();

/** Forgets the last Empty's summary so it does not linger over another listing. */
function clearSummary() {
  $('bin-status').textContent = '';
  $('bin-failures').replaceChildren();
  $('bin-failures').hidden = true;
}

function choose(event, which) {
  event.preventDefault();
  if (which === stage) return;
  stage = which;
  history.replaceState(null, '', `#${which}`);
  note('', false);
  selected = new Set();
  current = null;
  reload();
}

function note(text, isError) {
  error.classList.toggle('ok', !isError);
  showMessage(error, text);
}

/** Loads the stage again from the start, keeping the selection of entries still there. */
async function reload() {
  clearSummary();
  for (const which of ['first', 'second']) {
    if (which === stage) $(`stage-${which}`).setAttribute('aria-current', 'page');
    else $(`stage-${which}`).removeAttribute('aria-current');
  }
  $('stage-help').textContent =
    stage === 'first'
      ? `Deleted items stay here for ${BIN_DAYS} days. Restore puts an item back where it was. Delete moves it to the second stage, where only people who manage the site can restore it.`
      : 'Items removed from the first stage. Only people who manage the site see them. Delete forever erases an item for good.';
  const request = ++loading;
  const listed = await fetchEntries(null, request);
  if (!listed) return;
  entries = listed.items;
  next = listed.next;
  const ids = new Set(entries.map((entry) => entry.id));
  selected = new Set([...selected].filter((id) => ids.has(id)));
  render();
}

async function loadMore() {
  const request = ++loading;
  const listed = await fetchEntries(next, request);
  if (!listed) return;
  entries = entries.concat(listed.items);
  next = listed.next;
  render();
}

/** Up to LOAD entries of this stage after `after`; null when a newer listing was asked for or it failed. */
async function fetchEntries(after, request) {
  let items = [];
  let cursor = after;
  do {
    const query = `stage=${stage}&limit=${PAGE}${cursor ? `&after=${encodeURIComponent(cursor)}` : ''}`;
    const result = await api('GET', `/sites/${seg(siteId)}/bin?${query}`);
    if (request !== loading) return null;
    if (!result.ok) {
      note(messageFor(result.error), true);
      return null;
    }
    items = items.concat(result.data.items);
    cursor = result.data.next;
  } while (cursor && items.length < LOAD);
  return { items, next: cursor };
}

function applyFilters() {
  // Only what is still shown stays selected: the bar acts on what the person sees.
  const kept = new Set(filterBin(entries, { ...filters, me: me.id }).map((entry) => entry.id));
  selected = new Set([...selected].filter((id) => kept.has(id)));
  anchor = null;
  render({ keepFocus: false });
}

// ---- drawing ----

function render({ keepFocus = true } = {}) {
  const focusedRow = document.activeElement?.closest?.('#groups li.bin-row');
  const focusId = keepFocus && focusedRow ? focusedRow.dataset.id : null;
  const focusIndex = focusId ? shown.findIndex((entry) => entry.id === focusId) : -1;

  shown = filterBin(entries, { ...filters, me: me.id });
  const { timeZone } = regionalSettings();
  const groups = groupByDeletedDate(shown, Date.now(), timeZone);
  // The drawn order (by group) is the keyboard's order.
  shown = groups.flatMap((group) => group.items);
  if (!shown.some((entry) => entry.id === current)) current = shown[0]?.id ?? null;

  $('summary').textContent = binSummary(entries, { more: Boolean(next) });
  $('bin-filters').hidden = entries.length === 0;
  fillKinds();
  $('bin-bar').hidden = entries.length === 0;
  $('empty-bin').hidden = entries.length === 0;
  $('empty-bin').textContent = stage === 'second' ? 'Erase everything' : 'Empty';
  $('bulk-delete').textContent = stage === 'second' ? 'Delete selected forever' : 'Delete selected';
  $('more').hidden = !next;

  const narrowed = filtering(filters);
  $('empty').hidden = shown.length > 0;
  $('empty-text').textContent =
    entries.length === 0 ? emptyText(stage) : `Nothing matches${filters.text.trim() ? ` “${filters.text.trim()}”` : ' these filters'}.`;
  $('clear-filters').hidden = !(entries.length > 0 && narrowed);
  clearTimeout(shownTimer);
  // Said once typing pauses, not after every letter.
  shownTimer = setTimeout(() => {
    $('shown').textContent = shownText(shown.length, entries.length, filters);
  }, 400);

  $('groups').replaceChildren(
    ...groups.map((group) => {
      const headingId = `group-${group.id}`;
      return h(
        'section',
        { class: 'bin-group', 'aria-labelledby': headingId },
        h('h2', { id: headingId, class: 'bin-heading' }, group.heading, ' ', h('span', { class: 'meta' }, `(${group.items.length})`)),
        h('ul', { class: 'list bin-list', 'aria-labelledby': headingId }, ...group.items.map(row)),
      );
    }),
  );
  showSelection();
  resolvePlaces();

  if (focusId) {
    const target = rowFor(focusId) ?? rowFor(shown[Math.min(Math.max(focusIndex, 0), shown.length - 1)]?.id);
    if (target) goToRow(target);
  }
}

/** The type filter offers only the kinds in this stage. */
function fillKinds() {
  const select = $('bin-kind');
  const choices = kindChoices(entries);
  if (filters.kind && !choices.some((choice) => choice.value === filters.kind)) filters.kind = '';
  select.replaceChildren(
    h('option', { value: '' }, 'Any type'),
    ...choices.map((choice) => h('option', { value: choice.value }, choice.label)),
  );
  select.value = filters.kind;
}

function row(entry) {
  const isCurrent = entry.id === current;
  const tab = isCurrent ? '0' : '-1';
  const kind = binKind(entry);
  const box = h('input', { type: 'checkbox', class: 'bin-check', tabindex: tab, 'aria-label': `Select ${entry.name}` });
  box.checked = selected.has(entry.id);
  box.addEventListener('change', () => {
    anchor = null;
    current = entry.id;
    setSelection(toggleSelection(selected, entry.id));
  });
  const who = entry.deleted_by ? (entry.deleted_by.id === me.id ? 'you' : entry.deleted_by.name) : null;
  const when = relativeTime(entry.deleted_at, Date.now(), regionalSettings());
  const level = daysLeftLevel(entry.days_left);
  const left = daysLeftText(entry.days_left);
  const meter = h('meter', {
    class: `bin-meter ${level === 'ok' ? 'ok' : 'warn'}`,
    min: 0,
    max: BIN_DAYS,
    low: 8,
    high: BIN_DAYS,
    optimum: BIN_DAYS,
    value: Math.max(0, Math.min(BIN_DAYS, entry.days_left ?? 0)),
    'aria-label': `Time left before ${entry.name} is erased: ${left}`,
  });
  meter.textContent = left;
  const path = binPathText(entry);
  const li = h(
    'li',
    {
      class: `bin-row${selected.has(entry.id) ? ' checked' : ''}`,
      'data-id': entry.id,
      tabindex: tab,
      role: 'group',
      'aria-label': `${entry.name}, ${kind === 'folder' ? 'folder' : kind === 'library' ? 'library' : 'file'}, deleted ${when}, ${left}`,
    },
    box,
    fileIcon(kind),
    h(
      'span',
      { class: 'bin-main' },
      h('strong', { class: 'bin-name' }, entry.name),
      path ? h('span', { class: 'bin-path', 'data-path': pathKey(entry) }, path) : null,
      h(
        'small',
        { class: 'bin-who' },
        who ? `Deleted by ${who} · ` : 'Deleted ',
        h('time', { datetime: entry.deleted_at ?? '', title: formatDate(entry.deleted_at) }, when),
      ),
    ),
    h('small', { class: 'bin-size' }, formatSize(entry.size)),
    h('span', { class: `bin-left ${level}` }, meter, h('small', { 'aria-hidden': 'true' }, left)),
    h(
      'span',
      { class: 'actions' },
      action('Restore', entry, tab, () => restoreEntries([entry])),
      action(stage === 'second' ? 'Delete forever' : 'Delete', entry, tab, () => removeEntries([entry])),
    ),
  );
  li.addEventListener('focusin', () => makeCurrent(entry.id));
  return li;
}

function action(label, entry, tab, handler) {
  const button = h('button', { type: 'button', class: 'plain', tabindex: tab, 'aria-label': `${label} ${entry.name}` }, label);
  button.addEventListener('click', handler);
  return button;
}

/** Moves the tab stop to the row `id` without drawing the list again. */
function makeCurrent(id) {
  if (id === current) return;
  current = id;
  for (const li of rows()) {
    const tab = li.dataset.id === id ? '0' : '-1';
    li.tabIndex = Number(tab);
    for (const control of li.querySelectorAll('input, button, a')) control.tabIndex = Number(tab);
  }
}

function rows() {
  return [...$('groups').querySelectorAll('li.bin-row')];
}
function rowFor(id) {
  return id ? $('groups').querySelector(`li.bin-row[data-id="${CSS.escape(id)}"]`) : null;
}

function goToRow(li) {
  if (!li) return;
  makeCurrent(li.dataset.id);
  li.focus();
  li.scrollIntoView?.({ block: 'nearest' });
}

function selectedEntries() {
  return shown.filter((entry) => selected.has(entry.id));
}

function setSelection(ids) {
  selected = new Set(ids);
  for (const li of rows()) {
    const on = selected.has(li.dataset.id);
    li.classList.toggle('checked', on);
    const box = li.querySelector('input.bin-check');
    if (box) box.checked = on;
  }
  showSelection();
  $('keys-status').textContent = selected.size === 0 ? selectionText(0) : '';
}

function showSelection() {
  const count = selectedEntries().length;
  const all = $('select-all');
  all.disabled = shown.length === 0;
  all.checked = shown.length > 0 && count === shown.length;
  all.indeterminate = count > 0 && count < shown.length;
  $('selected-count').textContent = count ? selectionText(count) : '';
  $('bulk-restore').disabled = count === 0 || busy;
  $('bulk-delete').disabled = count === 0 || busy;
}

// ---- where an item was: a link to that folder if it is still there ----

function pathKey(entry) {
  return JSON.stringify(entry.path ?? []);
}

/** The libraries and folders in node `id`, all pages. */
function childrenOf(id) {
  if (!childLists.has(id)) {
    childLists.set(
      id,
      (async () => {
        let list = [];
        let after = null;
        do {
          const query = `limit=500${after ? `&after=${encodeURIComponent(after)}` : ''}`;
          const result = await api('GET', `/nodes/${seg(id)}/children?${query}`);
          if (!result.ok) return list;
          list = list.concat(result.data.items.filter((item) => item.kind === 'library' || item.kind === 'folder'));
          after = result.data.next;
        } while (after && list.length < 5000);
        return list;
      })(),
    );
  }
  return childLists.get(id);
}

/** Where the folder named by `path` (site first) is now, by walking down from the site; null if it is gone. */
function placeOf(path) {
  const key = JSON.stringify(path);
  if (!places.has(key)) {
    places.set(
      key,
      (async () => {
        if (path.length === 0) return null;
        let node = { id: siteId, kind: 'site' };
        for (const name of path.slice(1)) {
          const found = (await childrenOf(node.id)).find((child) => child.name === name);
          if (!found) return null;
          node = found;
        }
        return { href: pageFor(node.kind, node.id) };
      })().catch(() => null),
    );
  }
  return places.get(key);
}

/** Turns the quiet paths of the rows into links where the folder is still there. */
function resolvePlaces() {
  const spans = [...$('groups').querySelectorAll('span.bin-path[data-path]')];
  const keys = [...new Set(spans.map((span) => span.dataset.path))].slice(0, 50);
  for (const key of keys) {
    placeOf(JSON.parse(key)).then((place) => {
      if (!place) return;
      for (const span of $('groups').querySelectorAll('span.bin-path[data-path]')) {
        if (span.dataset.path !== key) continue;
        const li = span.closest('li.bin-row');
        const tab = li && li.dataset.id === current ? '0' : '-1';
        const link = h('a', { class: 'bin-path', href: place.href, tabindex: tab, 'aria-label': `Open ${span.textContent}` }, span.textContent);
        span.replaceWith(link);
      }
    });
  }
}

// ---- restore and remove ----

/** The next row to focus once `gone` (ids) leave the list. */
function rowAfter(gone) {
  const ids = new Set(gone);
  const at = shown.findIndex((entry) => entry.id === current);
  const after = shown.slice(at + 1).find((entry) => !ids.has(entry.id));
  const before = shown.slice(0, Math.max(at, 0)).reverse().find((entry) => !ids.has(entry.id));
  return (after ?? before)?.id ?? null;
}

/** After a change: the list again, focus on `focusId`'s row if focus was in the list. */
async function refresh(focusId) {
  const inList = Boolean(document.activeElement?.closest?.('#groups, #bin-bar')) || document.activeElement === document.body;
  if (focusId) current = focusId;
  await reload();
  if (inList) {
    const target = rowFor(current);
    if (target) goToRow(target);
    else if (!$('clear-filters').hidden) $('clear-filters').focus();
  }
}

async function restoreEntries(list) {
  if (busy || list.length === 0) return;
  busy = true;
  note('', false);
  showSelection();
  const restored = [];
  const failed = [];
  try {
    for (const entry of list) {
      const result = await api('POST', `/bin/${seg(entry.id)}/restore`);
      if (result.ok) restored.push({ entry, node: result.data.node, renamed: result.data.renamed });
      else failed.push({ entry, error: result.error, data: result.data });
    }
    // Where each went: the same place, unless a folder it was in is gone.
    const placeNames = new Map();
    const notes = [];
    for (const { entry, node, renamed } of restored) {
      let names = null;
      if (node?.parent) {
        if (!placeNames.has(node.parent)) {
          placeNames.set(
            node.parent,
            api('GET', `/nodes/${seg(node.parent)}`).then((r) => (r.ok ? r.data.path.map((crumb) => crumb.name) : null)),
          );
        }
        names = await placeNames.get(node.parent);
      } else if (node) names = [];
      notes.push(restoredNote(entry, names, renamed, { alone: restored.length === 1 }));
    }
    for (const key of places.keys()) places.delete(key); // folders may be back
    childLists.clear();
    if (restored.length) {
      const message = restoredText(restored.map((r) => r.entry), notes);
      const undo = buildUndo(
        { op: 'restore', done: restored.map((r) => r.node?.id ?? r.entry.id) },
        restored.map((r) => ({ id: r.node?.id ?? r.entry.id, name: r.node?.name ?? r.entry.name })),
      );
      offerUndo(undo, message);
    }
    if (failed.length) reportFailures('restored', failed);
  } finally {
    busy = false;
  }
  selected = new Set([...selected].filter((id) => !restored.some((r) => r.entry.id === id)));
  await refresh(rowAfter(restored.map((r) => r.entry.id)));
}

/** Says what was done with an Undo button (and Ctrl+Z) that reverses it, then how that went. */
function offerUndo(undo, message) {
  if (!undo) {
    showToast(message, { kind: 'ok' });
    return;
  }
  showToast(message, {
    announce: `${message} To undo, press ${UNDO_KEYS}.`,
    action: {
      label: undo.label,
      run: async () => {
        const outcome = await performUndo(undo, api);
        await refresh(current);
        showToast(outcome.message, { kind: outcome.ok ? 'ok' : 'error' });
      },
    },
  });
}

async function removeEntries(list) {
  if (busy || list.length === 0) return;
  note('', false);
  const second = stage === 'second';
  if (second) {
    const text =
      list.length === 1
        ? `Permanently erase “${list[0].name}”? Its files are erased from the server and this cannot be undone.`
        : eraseText(list.length);
    if (!(await confirmDialog(text, 'Delete forever'))) return;
  }
  busy = true;
  showSelection();
  const done = [];
  const failed = [];
  try {
    for (const entry of list) {
      const result = await api('DELETE', `/bin/${seg(entry.id)}`);
      if (result.ok) done.push(entry);
      else failed.push({ entry, error: result.error, data: result.data });
    }
  } finally {
    busy = false;
  }
  if (done.length) {
    const what = done.length === 1 ? `“${done[0].name}”` : `${done.length} items`;
    showToast(
      second
        ? `Erased ${what}.`
        : `Moved ${what} to the second stage. Only people who manage the site can restore ${done.length === 1 ? 'it' : 'them'} now.`,
      { kind: 'ok' },
    );
  }
  if (failed.length) reportFailures(second ? 'erased' : 'moved to the second stage', failed);
  selected = new Set([...selected].filter((id) => !done.some((entry) => entry.id === id)));
  await refresh(rowAfter(done.map((entry) => entry.id)));
}

/** A refusal in plain words: a bucket's own sentence (a blocked library, a held change) or the general one. */
function errorText(code, data) {
  return sourceWriteErrorText(code, data) || messageFor(code);
}

function reportFailures(verb, failed) {
  const lines = failed.slice(0, 3).map(({ entry, error: code, data }) => `${entry.name}: ${errorText(code, data)}`);
  if (failed.length > 3) lines.push(`and ${failed.length - 3} more.`);
  const head = failed.length === 1 ? `“${failed[0].entry.name}” could not be ${verb}.` : `${failed.length} items could not be ${verb}.`;
  note([head, ...lines].join(' '), true);
}

/** Every entry of this stage, all pages. */
async function allEntries() {
  let all = entries.slice();
  let after = next;
  while (after) {
    const query = `stage=${stage}&limit=500&after=${encodeURIComponent(after)}`;
    const result = await api('GET', `/sites/${seg(siteId)}/bin?${query}`);
    if (!result.ok) throw new Error(result.error);
    all = all.concat(result.data.items);
    after = result.data.next;
  }
  return all;
}

async function emptyBin() {
  note('', false);
  let all;
  try {
    all = await allEntries();
  } catch (e) {
    return note(messageFor(e.message), true);
  }
  if (all.length === 0) return;
  const label = stage === 'second' ? 'Erase everything' : 'Empty';
  if (!(await confirmDialog(emptyBinText(stage, all.length), label))) return;
  const verb = stage === 'second' ? 'erased' : 'moved to the second stage';
  const button = $('empty-bin');
  button.disabled = true;
  $('bin-failures').hidden = true;
  // Announced twice only: that it started, and how it ended.
  $('bin-status').textContent = `Working on ${all.length} ${all.length === 1 ? 'item' : 'items'}…`;
  let done = 0;
  const failed = [];
  try {
    for (const entry of all) {
      const result = await api('DELETE', `/bin/${seg(entry.id)}`);
      if (result.ok) done += 1;
      else failed.push({ name: entry.name, text: errorText(result.error, result.data) });
    }
  } finally {
    button.disabled = false;
  }
  await reload();
  $('bin-status').textContent = `${done} of ${all.length} ${all.length === 1 ? 'item was' : 'items were'} ${verb}.`;
  if (failed.length) {
    // The alert is for the failure summary only; the items are listed below it.
    note(`${failed.length} ${failed.length === 1 ? 'item' : 'items'} could not be ${verb}.`, true);
    $('bin-failures').replaceChildren(
      ...failed.map((f) => h('li', {}, h('span', { class: 'grow' }, h('strong', {}, f.name), h('br'), h('small', {}, f.text)))),
    );
    $('bin-failures').hidden = false;
  }
}

// ---- keyboard: the round-15 patterns on the bin's rows ----

/** The row a key press came from (the row itself), or null. */
function rowOf(event) {
  const li = event.target?.closest?.('#groups li.bin-row');
  return li && li === event.target ? li : null;
}

/** Whether keys for the rows are meant here: focus in the list or its bar, or on nothing in particular. */
function inBin(event) {
  const target = event.target;
  return target === document.body || Boolean(target?.closest?.('#groups, #bin-bar'));
}

function moveRow(event, extend) {
  const lis = rows();
  const li = rowOf(event);
  const at = lis.indexOf(li);
  const page = Math.max(1, Math.floor(innerHeight / (li?.offsetHeight || 64)) - 2);
  const to = moveIndex(event.key, at, lis.length, page);
  if (to === null) return false;
  const target = lis[to];
  if (extend) {
    if (!anchor || !rowFor(anchor)) {
      anchor = li.dataset.id;
      anchorBase = new Set(selected);
    }
    setSelection(extendSelection(lis.map((r) => r.dataset.id), anchorBase, anchor, target.dataset.id));
  } else anchor = null;
  goToRow(target);
}

function entryOf(id) {
  return shown.find((entry) => entry.id === id) ?? null;
}
function hasRows() {
  return shown.length > 0;
}

function registerKeys() {
  const scope = 'bin';
  addShortcut({
    keys: ['ArrowDown', 'ArrowUp'],
    label: 'Move to the next or previous item',
    scope,
    shown: hasRows,
    when: (event) => Boolean(rowOf(event)),
    run: (event) => moveRow(event, false),
  });
  addShortcut({
    keys: ['Home', 'End'],
    label: 'Move to the first or last item',
    scope,
    shown: hasRows,
    when: (event) => Boolean(rowOf(event)),
    run: (event) => moveRow(event, false),
  });
  addShortcut({
    keys: ['PageDown', 'PageUp'],
    label: 'Move a screen down or up',
    scope,
    shown: hasRows,
    when: (event) => Boolean(rowOf(event)),
    run: (event) => moveRow(event, false),
  });
  addShortcut({
    keys: ['Shift+ArrowDown', 'Shift+ArrowUp'],
    label: 'Select the items you move over',
    scope,
    shown: hasRows,
    when: (event) => Boolean(rowOf(event)),
    run: (event) => moveRow(event, true),
  });
  addShortcut({
    keys: ['Space'],
    label: 'Select or unselect the current item',
    scope,
    shown: hasRows,
    when: (event) => Boolean(rowOf(event)),
    run: (event) => {
      anchor = null;
      setSelection(toggleSelection(selected, event.target.dataset.id));
    },
  });
  addShortcut({
    keys: ['Mod+A'],
    label: 'Select every item shown',
    scope,
    shown: hasRows,
    when: (event) => hasRows() && inBin(event),
    run: () => {
      anchor = null;
      setSelection(shown.map((entry) => entry.id));
    },
  });
  addShortcut({
    keys: ['Escape'],
    label: 'Clear the selection',
    scope,
    shown: hasRows,
    when: (event, context) => !context.dialog && selected.size > 0 && inBin(event),
    run: () => {
      anchor = null;
      setSelection([]);
    },
  });
  addShortcut({
    keys: ['Enter'],
    label: 'Restore the current item',
    scope,
    shown: hasRows,
    when: (event) => Boolean(rowOf(event)) && !busy,
    run: (event) => {
      const entry = entryOf(event.target.dataset.id);
      if (entry) restoreEntries([entry]);
    },
  });
  addShortcut({
    keys: ['Delete'],
    label: 'Delete the selected items, or the current one (to the second stage; from there, for good after asking)',
    scope,
    shown: hasRows,
    when: (event) => inBin(event) && !busy && (selectedEntries().length > 0 || Boolean(entryOf(current))),
    run: () => {
      const list = selectedEntries();
      removeEntries(list.length ? list : [entryOf(current)]);
    },
  });
  addShortcut({
    keys: ['Mod+Z'],
    label: 'Undo the last restore',
    scope,
    when: (event) => isUndoKey(event),
    run: () => runLatestAction(),
  });
}

function fail(text) {
  settleAll();
  showMessage(error, text);
  throw new Error(text);
}
