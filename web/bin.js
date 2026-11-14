// The recycle bin of a site: what was deleted (the first stage, for whoever may restore it) and,
// for people who manage the site, the second stage. The server decides who sees and does what;
// this page lists, asks to confirm before anything is erased, and shows the answers in words.

import {
  api,
  bytesText,
  can,
  confirmDialog,
  daysLeftText,
  emptyBinText,
  formatTime,
  h,
  messageFor,
  offerRequest,
  queryId,
  requireMe,
  seg,
  setupTopbar,
  showCrumbs,
  showMessage,
} from './common.js';

const PAGE = 100;
const $ = (id) => document.getElementById(id);
const error = $('error');
const MISSING = 'This site does not exist, or you do not have access to it.';

// State the handlers below use. Declared before the start-up code that awaits.
let stage = location.hash === '#second' ? 'second' : 'first';
let entries = [];
let next = null;
let loading = 0; // the latest listing asked for; older answers are dropped

const me = await requireMe();
setupTopbar(me, error);
const siteId = queryId(location.search, 'site');
if (!siteId) fail(MISSING);
const here = await showCrumbs($('crumbs'), siteId, { linkLast: true });
if (!here) {
  offerRequest(error, siteId);
  fail(MISSING);
}
const manageSite = can(here.access, 'manage_site');
document.title = `Recycle bin — ${here.path[here.path.length - 1].name} — ShareCraft`;
$('stages').hidden = !manageSite;
if (!manageSite) stage = 'first';

$('stage-first').addEventListener('click', (event) => choose(event, 'first'));
$('stage-second').addEventListener('click', (event) => choose(event, 'second'));
$('more').addEventListener('click', () => loadPage());
$('empty-bin').addEventListener('click', emptyBin);
await reload();

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
  reload();
}

function note(text, isError) {
  error.classList.toggle('ok', !isError);
  showMessage(error, text);
}

async function reload() {
  clearSummary();
  entries = [];
  next = null;
  for (const which of ['first', 'second']) {
    if (which === stage) $(`stage-${which}`).setAttribute('aria-current', 'page');
    else $(`stage-${which}`).removeAttribute('aria-current');
  }
  $('stage-help').textContent =
    stage === 'first'
      ? 'Items you may restore. Delete moves an item to the second stage, where only people who manage the site can restore it.'
      : 'Items removed from the first stage. Only people who manage the site see them. Delete forever erases an item for good.';
  await loadPage();
}

async function loadPage() {
  const request = ++loading;
  const query = `stage=${stage}&limit=${PAGE}${next ? `&after=${encodeURIComponent(next)}` : ''}`;
  const result = await api('GET', `/sites/${seg(siteId)}/bin?${query}`);
  if (request !== loading) return;
  if (!result.ok) {
    note(messageFor(result.error), true);
    return;
  }
  entries = entries.concat(result.data.items);
  next = result.data.next;
  render();
}

function render() {
  $('empty').hidden = entries.length > 0;
  $('more').hidden = !next;
  $('empty-bin').hidden = entries.length === 0;
  $('empty-bin').textContent = stage === 'second' ? 'Erase everything' : 'Empty';
  $('entries').replaceChildren(...entries.map(row));
}

function row(entry) {
  const restore = action('Restore', entry, () => restoreEntry(entry));
  const remove = action(stage === 'second' ? 'Delete forever' : 'Delete', entry, () => removeEntry(entry));
  const where = entry.path.length ? `in ${entry.path.join(' / ')}` : 'at the top of its library';
  const by = entry.deleted_by ? `Deleted by ${entry.deleted_by.name}` : 'Deleted';
  return h(
    'li',
    {},
    h(
      'span',
      { class: 'grow' },
      h('strong', {}, entry.name),
      h('br'),
      h('small', {}, `${entry.kind === 'folder' ? 'Folder' : 'File'} ${where}`),
    ),
    h('small', {}, `${by}${entry.deleted_at ? `, ${formatTime(entry.deleted_at)}` : ''}`),
    h('small', {}, daysLeftText(entry.days_left)),
    h('small', {}, bytesText(entry.size)),
    h('span', { class: 'actions' }, restore, remove),
  );
}

function action(label, entry, handler) {
  const button = h('button', { type: 'button', class: 'plain', 'aria-label': `${label} ${entry.name}` }, label);
  button.addEventListener('click', handler);
  return button;
}

async function restoreEntry(entry) {
  note('', false);
  const result = await api('POST', `/bin/${seg(entry.id)}/restore`);
  if (!result.ok) return note(messageFor(result.error), true);
  const renamed = result.data.renamed;
  note(
    renamed
      ? `“${renamed.from}” was restored as “${renamed.to}”, because its name was taken.`
      : `“${entry.name}” was restored.`,
    false,
  );
  await reload();
}

async function removeEntry(entry) {
  note('', false);
  if (stage === 'second') {
    const sure = await confirmDialog(
      `Permanently erase “${entry.name}”? Its files are erased from the server and this cannot be undone.`,
      'Delete forever',
    );
    if (!sure) return;
  }
  const result = await api('DELETE', `/bin/${seg(entry.id)}`);
  if (!result.ok) return note(messageFor(result.error), true);
  note(
    stage === 'second'
      ? `“${entry.name}” was erased.`
      : `“${entry.name}” was moved to the second stage. Only people who manage the site can restore it now.`,
    false,
  );
  await reload();
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
      else failed.push({ name: entry.name, text: messageFor(result.error) });
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

function fail(text) {
  showMessage(error, text);
  throw new Error(text);
}
