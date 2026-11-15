// A site's home page: a short introduction (description, kind, hub, owners), the libraries as
// cards and the files changed lately across them. Everything administrative sits in one
// "Manage site" menu for the people who may use it; members see none of it.

import {
  api,
  directoryRow,
  formatDate,
  h,
  kindLabel,
  libraryKindChoices,
  messageFor,
  offerRequest,
  pageFor,
  queryId,
  can,
  confirmDialog,
  deleteLibraryText,
  quotaSubmit,
  quotaText,
  usagePercent,
  requireMe,
  seg,
  setupTopbar,
  showCrumbs,
  showMessage,
  siteChrome,
} from './common.js';
import { setupMenu } from './menu.js';
import { showToast } from './toast.js';
import { buildUndo, performUndo } from './undo.js';
import { relativeTime } from './files.js';
import { libraryCardModel, libraryIcon, ownerChip, recentAcrossLibraries, recentIcon } from './site-home.js';
import { checklistModel, readWelcome, showChecklist, unsetParts, unsetText, writeWelcome } from './new-site.js';

const $ = (id) => document.getElementById(id);
const error = $('error');

/** How many libraries' first pages "Recent in this site" reads, and how many kinds are asked. */
const RECENT_LIBRARIES = 6;
const KINDS_ASKED = 24;
const RECENT_SHOWN = 8;

const me = await requireMe();
setupTopbar(me, error);
const siteId = queryId(location.search);
const MISSING = 'This site does not exist, or you do not have access to it.';
const ARCHIVED = 'This site is archived. It can be read but not changed.';
if (!siteId) fail(MISSING);

const here = await showCrumbs($('crumbs'), siteId, { linkLast: false });
if (!here) {
  offerRequest(error, siteId);
  fail(MISSING);
}
const site = here.path[here.path.length - 1];
let info = (await siteChrome(siteId, { currentId: siteId, footer: true })).site;
$('title').textContent = site.name;
document.title = `${site.name} — ShareCraft`;
const manageSite = can(here.access, 'manage_site');
const manageAccess = can(here.access, 'manage_access');

// What was done is said in a toast (refusals stay in the dialog or above the page).
const announce = (text) => showToast(text);

// Get started (round 24, drawn further down): arriving from the New site wizard (`welcome=1`),
// and the parts it could not set (`unset`). Both leave the address once read.
const params = new URLSearchParams(location.search);
const welcome = params.get('welcome') === '1';
const unset = unsetParts(params.get('unset'));
if (welcome || params.has('unset')) history.replaceState(null, '', `${location.pathname}?id=${seg(siteId)}${location.hash}`);
let libraryFacts = null; // what showLibraries found: { count, files, first }
let checklistFacts = null; // people, look and hubs, read for owners only
let newLibraryButton = null; // the New library card's button, which the checklist opens too
const mayGetStarted = () => manageSite && !info?.archived && !readWelcome(me.id, siteId).dismissed;

// ---- Manage site: one menu, shown only to people who may use something in it ----

$('settings-link').href = `/site-settings.html?id=${seg(siteId)}`;
$('look-link').href = `/site-look.html?id=${seg(siteId)}`;
$('access-link').href = `/access.html?id=${seg(siteId)}`;
$('groups-link').href = `/groups.html?site=${seg(siteId)}`;
$('explorer-link').href = `/explorer.html?root=${seg(siteId)}`;
$('bin-link').href = `/bin.html?site=${seg(siteId)}`;
$('settings-link').hidden = !(manageSite || me.is_admin);
$('look-link').hidden = !manageSite;
$('access-link').hidden = !manageAccess;
$('groups-link').hidden = !manageSite;
$('explorer-link').hidden = !manageAccess;
$('bin-link').hidden = !(manageSite || manageAccess);
// Usage is shown to people who manage the site; administrators may set the quota anywhere.
$('quota-item').hidden = !(manageSite || me.is_admin);
const mayManage = manageSite || manageAccess || me.is_admin;
$('manage-wrap').hidden = !mayManage;
if (mayManage) setupMenu($('manage-button'), $('manage-menu'));

/** Opens `dialog` with `first` focused; focus goes back to the Manage site button when it closes. */
function openDialog(dialog, first, back = $('manage-button')) {
  dialog.showModal();
  first?.focus();
  dialog.addEventListener('close', () => back?.isConnected && back.focus(), { once: true });
}

// ---- the introduction: description, kind and hub as chips, owners as avatar chips ----

/**
 * What the site is: its description, kind, hub and owners, and the archived banner. Also whether
 * the hub entry of Manage site is offered: people who manage the site may join it to a hub or
 * leave the one it joined; a hub joins none, and an archived site cannot change its hub.
 */
function showAbout() {
  if (!info) return;
  $('about').hidden = false;
  $('description').textContent = info.description ?? '';
  $('description').hidden = !info.description;
  $('kind-chip').textContent = kindLabel(info.kind);
  const hubChip = $('hub-chip');
  hubChip.hidden = !(info.is_hub || info.hub);
  if (info.is_hub) hubChip.replaceChildren('Hub: other sites can join it');
  else if (info.hub) hubChip.replaceChildren('In the hub ', h('a', { href: pageFor('site', info.hub.id) }, info.hub.name));
  const owners = info.owners ?? [];
  $('owners-row').hidden = owners.length === 0;
  $('owners').replaceChildren(...owners.map((name) => ownerChip(name)));
  $('archived-banner').hidden = !info.archived;
  if (info.archived?.at) {
    $('archived-banner').replaceChildren(
      ARCHIVED,
      ' ',
      h('small', {}, `Archived ${formatDate(info.archived.at)}${info.archived.by ? ` by ${info.archived.by}` : ''}.`),
    );
  }
  $('hub-item').hidden = !(can(info.access, 'manage_site') && !info.is_hub && !info.archived);
  // Owners are told on the page itself when the site is nearly full; the figures are in Storage quota.
  const warning = manageSite && info.used !== undefined && info.quota_warning;
  $('storage-warning').hidden = !warning;
  if (warning) $('storage-warning').textContent = quotaText(info);
}

/** Reads the site and its bars again (after its hub or quota changed) and draws the introduction. */
async function refreshSite(fallback) {
  info = (await siteChrome(siteId, { currentId: siteId, footer: true })).site ?? fallback ?? info;
  showAbout();
  showGetStarted();
}

showAbout();

// ---- Join or leave a hub… ----

$('hub-item').addEventListener('click', async () => {
  showMessage($('hub-error'), '');
  const joined = info?.hub;
  $('leave-hub').hidden = !joined;
  $('join-hub').hidden = true;
  $('hub-field').hidden = true;
  $('hub-close').textContent = 'Cancel';
  if (joined) {
    $('hub-text').replaceChildren('This site has joined the hub ', h('strong', {}, joined.name), '. Leaving it takes the hub’s navigation off this site.');
    openDialog($('hub-dialog'), $('hub-close'));
    return;
  }
  const hubs = await api('GET', '/hubs');
  const choices = hubs.ok ? hubs.data.filter((hub) => hub.id !== siteId) : [];
  $('hub-choice').replaceChildren(...choices.map((hub) => h('option', { value: hub.id }, hub.name)));
  $('hub-field').hidden = choices.length === 0;
  $('join-hub').hidden = choices.length === 0;
  if (!hubs.ok) $('hub-text').textContent = messageFor(hubs.error);
  else if (choices.length === 0) {
    $('hub-text').textContent = 'There is no hub you can join this site to.';
    $('hub-close').textContent = 'Close';
  } else $('hub-text').textContent = 'A site in a hub shows the hub’s navigation and is listed on the hub’s page.';
  openDialog($('hub-dialog'), choices.length ? $('hub-choice') : $('hub-close'));
});

/** Joins the hub `hub`, or leaves the one joined (`null`); refusals stay in the dialog. */
async function setHub(hub) {
  showMessage($('hub-error'), '');
  const result = await api('PUT', `/sites/${seg(siteId)}/joined-hub`, { hub });
  if (!result.ok) return showMessage($('hub-error'), messageFor(result.error));
  $('hub-dialog').close();
  await refreshSite(result.data);
  announce(info?.hub ? `This site joined the hub ${info.hub.name}.` : 'This site left its hub.');
}

$('hub-close').addEventListener('click', () => $('hub-dialog').close());
$('hub-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  if ($('hub-choice').value) await setHub($('hub-choice').value);
});
$('leave-hub').addEventListener('click', () => setHub(null));

// ---- Storage quota… ----

/**
 * Fills the storage dialog: for people who manage the site what it holds against its quota
 * (warning from 90%); administrators get the field to set or remove the quota too, and where
 * they do not manage the site, the field without the figures.
 */
async function fillStorage() {
  let data = info;
  if (manageSite && (!data || data.used === undefined)) {
    const result = await api('GET', `/sites/${seg(siteId)}`);
    if (result.ok) data = result.data;
  }
  const known = manageSite && data && data.used !== undefined;
  const text = $('storage-text');
  text.textContent = known ? quotaText(data) : 'The current quota and usage are shown to people who manage this site.';
  text.className = known && data.quota_warning ? 'note' : 'meta';
  const percent = known ? usagePercent(data.used, data.quota) : null;
  const bar = $('storage-bar');
  bar.hidden = percent === null;
  if (percent !== null) {
    bar.value = percent;
    bar.textContent = `${percent}%`;
    bar.setAttribute('aria-label', `${percent}% of the storage quota used`);
    bar.classList.toggle('warn', Boolean(data.quota_warning));
  }
  for (const id of ['quota-field', 'quota-set']) $(id).hidden = !me.is_admin;
  // There is nothing to remove where the site is known to have no quota.
  $('quota-remove').hidden = !me.is_admin || (known && data.quota == null);
  $('quota-close').textContent = me.is_admin ? 'Cancel' : 'Close';
  if (me.is_admin) $('quota-gb').value = known && data.quota != null ? String(Math.round((data.quota / 1024 ** 3) * 100) / 100) : '';
}

$('quota-item').addEventListener('click', async () => {
  showMessage($('quota-error'), '');
  await fillStorage();
  openDialog($('quota-dialog'), me.is_admin ? $('quota-gb') : $('quota-close'));
});
$('quota-close').addEventListener('click', () => $('quota-dialog').close());

/** Sets the quota to `bytes` (null for none); the dialog closes and says what changed. */
async function putQuota(bytes) {
  const result = await api('PUT', `/sites/${seg(siteId)}/quota`, { bytes });
  if (!result.ok) return showMessage($('quota-error'), messageFor(result.error));
  $('quota-dialog').close();
  if (manageSite) await refreshSite();
  announce(bytes === null ? 'The quota was removed.' : 'The quota was set.');
}

$('quota-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  showMessage($('quota-error'), '');
  const asked = quotaSubmit($('quota-gb').value, { blind: true });
  if (asked.error) return showMessage($('quota-error'), asked.error);
  await putQuota(asked.bytes);
});

$('quota-remove').addEventListener('click', async () => {
  showMessage($('quota-error'), '');
  const sure = await confirmDialog('Remove the storage quota of this site? It will then have no limit.', 'Remove quota');
  if (sure) await putQuota(null);
  else $('quota-remove').focus();
});

// ---- New library: a card-shaped button opening a dialog ----

// A new library is a document, image or template library; administrators may make asset libraries.
$('library-kind').replaceChildren(
  ...libraryKindChoices(me.is_admin).map((choice) => h('option', { value: choice.value }, choice.label)),
);
const mayAddLibrary = () => manageSite && !info?.archived;

$('new-library-cancel').addEventListener('click', () => $('new-library-dialog').close());
$('new-library').addEventListener('submit', async (event) => {
  event.preventDefault();
  const say = (text) => showMessage($('new-library-error'), text);
  const name = $('library-name').value.trim();
  if (!name) {
    say('Give the library a name.');
    $('library-name').focus();
    return;
  }
  say('');
  $('new-library-submit').disabled = true;
  const result = await api('POST', `/sites/${seg(siteId)}/libraries`, { name, kind: $('library-kind').value });
  $('new-library-submit').disabled = false;
  if (result.ok) location.assign(pageFor('library', result.data.id));
  else say(messageFor(result.error));
});

function newLibraryCard() {
  const button = h(
    'button',
    { type: 'button', class: 'new-card' },
    h('span', { class: 'new-plus', 'aria-hidden': 'true' }, '+'),
    'New library',
  );
  button.addEventListener('click', () => {
    $('library-name').value = '';
    $('library-kind').value = 'document';
    showMessage($('new-library-error'), '');
    openDialog($('new-library-dialog'), $('library-name'), button);
  });
  newLibraryButton = button;
  return h('li', { class: 'library-card new' }, button);
}

// ---- the libraries as cards, and the files changed lately across them ----

/** The libraries of the site the person can see, their kinds and first pages, then both sections. */
async function showLibraries() {
  const result = await api('GET', `/nodes/${seg(siteId)}/children?limit=500`);
  if (!result.ok) {
    showMessage(error, messageFor(result.error));
    return;
  }
  const libraries = result.data.items.filter((item) => item.kind === 'library');
  const read = libraries.slice(0, RECENT_LIBRARIES);
  // The kinds (a library the person sees only in part answers nothing) and a few first pages, together.
  const [kinds, pages] = await Promise.all([
    Promise.all(
      libraries.map((library, index) =>
        library.access && index < KINDS_ASKED ? api('GET', `/libraries/${seg(library.id)}`) : null,
      ),
    ),
    Promise.all(read.map((library) => api('GET', `/nodes/${seg(library.id)}/children?limit=100`))),
  ]);
  const models = libraries.map((library, index) => {
    const page = index < read.length && pages[index].ok ? pages[index].data : null;
    return libraryCardModel({
      ...library,
      kind: kinds[index]?.ok ? kinds[index].data.kind : undefined,
      count: page ? page.items.length : undefined,
      more: Boolean(page?.next),
    });
  });
  libraryFacts = {
    count: libraries.length,
    files: read.some((library, index) => pages[index].ok && pages[index].data.items.some((item) => item.kind === 'document')),
    first: libraries.find((library) => library.access) ?? null,
  };
  $('empty').hidden = libraries.length > 0 || mayAddLibrary();
  $('libraries').replaceChildren(...models.map(libraryCard), ...(mayAddLibrary() ? [newLibraryCard()] : []));
  showRecent(read.map((library, index) => ({ library, items: pages[index].ok ? pages[index].data.items : [] })));
  showGetStarted();
}

/** One library card: its icon, name (the card's one link), kind, count and access; owners get Actions. */
function libraryCard(model) {
  const marks = [
    model.limited ? h('span', { class: 'mark' }, 'Limited access') : null,
    model.ownPermissions ? h('span', { class: 'mark' }, 'Own permissions') : null,
  ].filter(Boolean);
  const facts = [model.kind, model.count].filter(Boolean).join(' · ');
  return h(
    'li',
    { class: 'library-card' },
    h('span', { class: 'lib-pic' }, libraryIcon(model.icon)),
    h(
      'div',
      { class: 'lib-body' },
      h('h3', {}, h('a', { class: 'card-link', href: model.href }, model.name)),
      h('p', { class: 'lib-facts' }, facts),
      marks.length ? h('p', { class: 'card-badges' }, ...marks) : null,
    ),
    // People who manage the site may delete a library (to the recycle bin), unless it is archived.
    mayAddLibrary() ? libraryActions(model) : null,
  );
}

/** The small Actions menu of a library card: Delete, which asks first, naming the library. */
function libraryActions(model) {
  const menuId = `library-menu-${model.id}`;
  const button = h(
    'button',
    { type: 'button', class: 'plain menu-button dots', 'aria-label': `Actions for ${model.name}` },
    h('span', { 'aria-hidden': 'true' }, '⋯'),
  );
  const remove = h('button', { type: 'button', role: 'menuitem', class: 'danger' }, 'Delete');
  const menu = h('ul', { id: menuId, class: 'menu end', 'aria-label': `Actions for ${model.name}` }, h('li', { role: 'none' }, remove));
  remove.addEventListener('click', async () => {
    showMessage(error, '');
    if (!(await confirmDialog(deleteLibraryText(model.name), 'Delete library'))) return button.focus();
    const result = await api('DELETE', `/nodes/${seg(model.id)}`);
    if (!result.ok) {
      showMessage(error, messageFor(result.error));
      return button.focus();
    }
    info = (await siteChrome(siteId, { currentId: siteId, footer: true })).site ?? info;
    showAbout();
    await showLibraries();
    $('libraries').querySelector('.card-link, .new-card')?.focus();
    // Asked first, then undoable for a while: Undo restores it from the recycle bin.
    const undo = buildUndo({ op: 'delete', done: [model.id] }, [{ id: model.id, name: model.name }]);
    showToast(`Deleted the library ${model.name}`, {
      action: {
        label: 'Undo',
        run: async () => {
          const outcome = await performUndo(undo, api);
          info = (await siteChrome(siteId, { currentId: siteId, footer: true })).site ?? info;
          showAbout();
          await showLibraries();
          showToast(outcome.message, { kind: outcome.ok ? 'ok' : 'error' });
        },
      },
    });
  });
  const wrap = h('div', { class: 'menu-wrap lib-actions' }, button, menu);
  setupMenu(button, menu);
  return wrap;
}

/** "Recent in this site": the files changed last in the first pages of `lists`, with their library. */
function showRecent(lists) {
  const files = recentAcrossLibraries(lists, RECENT_SHOWN);
  $('recent-empty').hidden = files.length > 0;
  $('recent').replaceChildren(
    ...files.map((file) =>
      h(
        'li',
        { class: 'recent-file' },
        recentIcon(file),
        h(
          'span',
          { class: 'recent-text' },
          h('a', { href: file.href, title: file.name }, file.name),
          h(
            'small',
            {},
            file.libraryName ? `${file.libraryName} · ` : '',
            h('time', { datetime: file.at, title: formatDate(file.at) }, relativeTime(file.at)),
          ),
        ),
      ),
    ),
  );
}

// ---- Get started: a checklist for the owners of a new site (round 24) ----

/** People, the look and the hubs, for the checklist: read once, again after something changed them. */
async function readChecklistFacts() {
  const [groups, access, look, hubs] = await Promise.all([
    api('GET', `/sites/${seg(siteId)}/groups`),
    manageAccess ? api('GET', `/nodes/${seg(siteId)}/access`) : null,
    api('GET', `/sites/${seg(siteId)}/look/settings`),
    api('GET', '/hubs'),
  ]);
  const siteGroups = groups.ok ? groups.data : [];
  const inGroups = siteGroups.reduce((sum, group) => sum + (Number(group.member_count) || 0), 0);
  // People and groups given access on the site itself, besides its own three groups (links are not people).
  const own = new Set(siteGroups.map((group) => group.id));
  const given = access?.ok ? access.data.assignments.filter((a) => a.principal.kind !== 'link' && !own.has(a.principal.id)).length : 0;
  checklistFacts = {
    people: inGroups + given,
    lookSet: look.ok ? look.data.look != null : false,
    hubs: hubs.ok ? hubs.data.filter((hub) => hub.id !== siteId).length : 0,
  };
}

/** Draws the Get started card, or hides it (see showChecklist). */
function showGetStarted() {
  const card = $('get-started');
  if (!checklistFacts || !libraryFacts || !mayGetStarted()) {
    card.hidden = true;
    return;
  }
  const model = checklistModel(info, {
    ...checklistFacts,
    files: libraryFacts.files,
    libraries: libraryFacts.count,
    baseline: readWelcome(me.id, siteId).baseline ?? undefined,
  });
  card.hidden = !showChecklist({ manages: manageSite, archived: Boolean(info?.archived), dismissed: false, welcome, complete: model.complete });
  if (card.hidden) return;
  $('gs-bar').max = model.total;
  $('gs-bar').value = model.done;
  $('gs-summary').textContent = model.summary;
  $('gs-list').replaceChildren(...model.items.map(checklistItem));
}

/** One step of the checklist: a tick, what to do (a link, or a button for a dialog) and a hint. */
function checklistItem(item) {
  const label = `${item.label}${item.done ? ' (done)' : ''}`;
  const link = (href) => h('a', { class: 'gs-action', href }, label);
  const button = (run) => {
    const element = h('button', { type: 'button', class: 'gs-action' }, label);
    element.addEventListener('click', run);
    return element;
  };
  let action;
  if (item.key === 'people') action = link(manageAccess ? `/access.html?id=${seg(siteId)}` : `/groups.html?site=${seg(siteId)}`);
  else if (item.key === 'upload') {
    action = libraryFacts.first ? link(`${pageFor('library', libraryFacts.first.id)}#upload`) : button(() => newLibraryButton?.click());
  } else if (item.key === 'look') action = link(`/site-look.html?id=${seg(siteId)}`);
  else if (item.key === 'hub') action = button(() => $('hub-item').click());
  else action = button(() => newLibraryButton?.click());
  return h(
    'li',
    { class: `gs-item${item.done ? ' done' : ''}` },
    h('span', { class: 'gs-tick', 'aria-hidden': 'true' }, item.done ? '✓' : ''),
    h('span', { class: 'gs-text' }, action, h('span', { class: 'gs-hint' }, item.done ? 'Done.' : item.hint)),
  );
}

$('gs-dismiss').addEventListener('click', () => {
  writeWelcome(me.id, siteId, { dismissed: true });
  $('get-started').hidden = true;
  $('title').tabIndex = -1;
  $('title').focus();
  showToast('Get started is dismissed. Everything in it is in Manage site.');
});

if (mayGetStarted()) await readChecklistFacts();
await showLibraries();
// What the wizard could not set on the new site is said once, with the way to try again.
if (unset.length) {
  showToast(unsetText(unset), {
    kind: 'error',
    action: {
      label: 'Try again in Site settings',
      run: () => location.assign(`/site-settings.html?id=${seg(siteId)}#${unset[0]}`),
    },
  });
}
setInterval(() => {
  for (const time of $('recent').querySelectorAll('time')) time.textContent = relativeTime(time.dateTime);
}, 60_000);

// ---- on a hub's page: the sites that joined it which the person can see ----

if (info?.is_hub) {
  const result = await api('GET', `/hubs/${seg(siteId)}/sites`);
  if (result.ok) {
    $('members').hidden = false;
    $('members-empty').hidden = result.data.length > 0;
    $('member-sites').replaceChildren(...result.data.map(directoryRow));
  }
}

function fail(text) {
  showMessage(error, text);
  throw new Error(text);
}
