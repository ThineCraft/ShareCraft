// A site's settings, for the people who manage it: its name, description, language and time
// zone, a link to Change the look, the hub it joined, and its own navigation (Automatic, or a custom list edited with
// buttons). Administrators also make it a hub and edit the hub's bar, unregister it, and save
// the site as a template. The server decides every change; refusals are shown as sentences.

import {
  api,
  can,
  confirmDialog,
  fillSelect,
  h,
  languageChoices,
  messageFor,
  pageFor,
  queryId,
  requireMe,
  seg,
  setupTopbar,
  showCrumbs,
  showMessage,
  siteChrome,
  templateRefusal,
  timeZoneChoices,
} from './common.js';
import { navigationBody, navigationEditor, newItem, placePicker } from './navigation.js';

const $ = (id) => document.getElementById(id);
const error = $('error');
const MISSING = 'This site does not exist, or you do not have access to it.';
let localEditor = null;

let hubEditor = null;


const me = await requireMe();
setupTopbar(me, error);
const siteId = queryId(location.search);
if (!siteId) fail(MISSING);
if (!(await showCrumbs($('crumbs'), siteId))) fail(MISSING);
let info = (await siteChrome(siteId)).site;
if (!info) fail(MISSING);
showTitle();

const manageSite = can(info.access, 'manage_site');
$('archived-banner').hidden = !info.archived;
if (!info.archived && !manageSite && !me.is_admin) {
  fail('Only the owners of this site and administrators can change its settings.');
}

/** Shows `text` in the message paragraph `id`; `ok` for a confirmation rather than a refusal. */
function note(id, text, ok = false) {
  $(id).classList.toggle('ok', ok);
  showMessage($(id), text);
}

function showTitle() {
  $('title').textContent = `Settings of ${info.name}`;
  document.title = `${info.name} settings — ShareCraft`;
}

/** Draws the bars again (a new name, hub or navigation) and keeps the site as now answered. */
async function redrawChrome() {
  info = (await siteChrome(siteId)).site ?? info;
  showTitle();
}

// The look is shown to the people who manage the site, archived or not (it is read-only then).
if (manageSite) {
  $('look').hidden = false;
  $('look-link').setAttribute('href', `/site-look.html?id=${seg(siteId)}`);
}

// An archived site can be read but not changed; administrators may still save it as a template.
if (!info.archived && manageSite) {
  showGeneral();
  await showHub();
  await showNavigation();
}
if (me.is_admin) {
  if (!info.archived) await showHubAdmin();
  $('template').hidden = false;
}

// ---- name, description, language and time zone ----

function showGeneral() {
  $('general').hidden = false;
  $('site-name').value = info.name;
  $('site-description').value = info.description ?? '';
  fillSelect($('site-language'), languageChoices(info.language), info.language);
  fillSelect($('site-zone'), timeZoneChoices(info.time_zone), info.time_zone);
}

$('settings').addEventListener('submit', async (event) => {
  event.preventDefault();
  note('settings-msg', '');
  const wanted = {
    name: $('site-name').value.trim(),
    description: $('site-description').value,
    language: $('site-language').value,
    time_zone: $('site-zone').value,
  };
  const change = Object.fromEntries(Object.entries(wanted).filter(([field, value]) => value !== (info[field] ?? '')));
  if (Object.keys(change).length === 0) return note('settings-msg', 'Nothing has changed.', true);
  const result = await api('PATCH', `/sites/${seg(siteId)}`, change);
  if (!result.ok) return note('settings-msg', messageFor(result.error));
  await redrawChrome();
  showGeneral();
  note('settings-msg', 'Saved.', true);
});

// ---- the hub this site joined ----

async function showHub() {
  $('hub').hidden = false;
  if (info.is_hub) $('hub-line').replaceChildren('This site is a hub. Other sites can join it; a hub joins no other hub.');
  else if (info.hub) $('hub-line').replaceChildren('This site is in the hub ', h('a', { href: pageFor('site', info.hub.id) }, info.hub.name), '.');
  else $('hub-line').replaceChildren('This site is in no hub you can see.');
  $('leave-hub-row').hidden = !(info.hub && !info.is_hub);
  $('join-hub').hidden = true;
  $('join-note').hidden = true;
  if (info.is_hub || info.hub) return;
  const hubs = await api('GET', '/hubs');
  const choices = hubs.ok ? hubs.data.filter((hub) => hub.id !== siteId) : [];
  $('hub-choice').replaceChildren(...choices.map((hub) => h('option', { value: hub.id }, hub.name)));
  $('join-hub').hidden = choices.length === 0;
  $('join-note').hidden = choices.length === 0;
}

/** Joins the hub `hub`, or leaves the one joined (`null`). */
async function setHub(hub) {
  note('hub-msg', '');
  const result = await api('PUT', `/sites/${seg(siteId)}/joined-hub`, { hub });
  if (!result.ok) return note('hub-msg', messageFor(result.error));
  await redrawChrome();
  await showHub();
  note('hub-msg', hub ? 'The site joined the hub.' : 'The site left the hub.', true);
}

$('join-hub').addEventListener('submit', async (event) => {
  event.preventDefault();
  if ($('hub-choice').value) await setHub($('hub-choice').value);
});
$('leave-hub').addEventListener('click', () => setHub(null));

// ---- navigation ----

/** The site's libraries, `[{id, name, kind}]`. */
async function libraries() {
  const result = await api('GET', `/nodes/${seg(siteId)}/children?limit=500`);
  return result.ok ? result.data.items.filter((item) => item.kind === 'library') : [];
}

/** The groups an item may be aimed at; `site` limits them to the organisation's and that site's. */
async function audienceGroups(site) {
  const result = await api('GET', '/groups');
  if (!result.ok) return [];
  return site ? result.data.filter((group) => !group.site || group.site.id === site) : result.data;
}

async function showNavigation() {
  const answer = await api('GET', `/sites/${seg(siteId)}/navigation?edit=1`);
  if (!answer.ok) return note('navigation-msg', messageFor(answer.error));
  $('navigation').hidden = false;
  const automatic = answer.data.local == null;
  // Choosing Custom starts from what Automatic shows: each library, under its own name.
  const items = automatic
    ? (await libraries()).map((library) => newItem({ label: '', target: { node: library.id, name: library.name } }))
    : answer.data.local;
  localEditor = navigationEditor({
    items,
    groups: await audienceGroups(siteId),
    picker: placePicker({ first: libraries }),
    label: 'Links of this site',
  });
  $('local-editor').replaceChildren(localEditor.element);
  $('nav-automatic').checked = automatic;
  $('nav-custom').checked = !automatic;
  $('local-editor').hidden = automatic;
}

for (const radio of [$('nav-automatic'), $('nav-custom')]) {
  radio.addEventListener('change', () => {
    $('local-editor').hidden = $('nav-automatic').checked;
  });
}

$('save-navigation').addEventListener('click', async () => {
  note('navigation-msg', '');
  if (!localEditor) return;
  const body = navigationBody({ automatic: $('nav-automatic').checked, items: localEditor.items() });
  if (body.error) return note('navigation-msg', body.error);
  const result = await api('PUT', `/sites/${seg(siteId)}/navigation`, body);
  if (!result.ok) return note('navigation-msg', messageFor(result.error));
  await redrawChrome();
  note('navigation-msg', 'The navigation was saved.', true);
});

// ---- hub settings, for administrators ----

/** The sites, for links in a hub's bar: `[{id, name, kind}]`. */
async function sites() {
  const result = await api('GET', '/sites');
  return result.ok ? result.data.map((site) => ({ id: site.id, name: site.name, kind: 'site' })) : [];
}

async function showHubAdmin() {
  $('hub-admin').hidden = false;
  $('make-hub-row').hidden = info.is_hub;
  $('hub-bar').hidden = !info.is_hub;
  if (!info.is_hub) return;
  const answer = await api('GET', `/sites/${seg(siteId)}/navigation?edit=1`);
  if (!answer.ok) return note('hub-admin-msg', messageFor(answer.error));
  hubEditor = navigationEditor({
    items: answer.data.hub ?? [],
    groups: await audienceGroups(null),
    picker: placePicker({ first: sites, label: 'Site, library or folder' }),
    label: 'Links of the hub bar',
  });
  $('hub-editor').replaceChildren(hubEditor.element);
}

$('make-hub').addEventListener('click', async () => {
  note('hub-admin-msg', '');
  const result = await api('PUT', `/sites/${seg(siteId)}/hub`, { navigation: [] });
  if (!result.ok) return note('hub-admin-msg', messageFor(result.error));
  info = result.data;
  await redrawChrome();
  await showHubAdmin();
  if (manageSite) await showHub();
  note('hub-admin-msg', 'This site is now a hub. Add links to its bar below.', true);
});

$('save-hub').addEventListener('click', async () => {
  note('hub-admin-msg', '');
  if (!hubEditor) return;
  const body = navigationBody({ automatic: false, items: hubEditor.items() });
  if (body.error) return note('hub-admin-msg', body.error);
  const result = await api('PUT', `/sites/${seg(siteId)}/hub`, { navigation: body.items });
  if (!result.ok) return note('hub-admin-msg', messageFor(result.error));
  await redrawChrome();
  note('hub-admin-msg', 'The hub bar was saved.', true);
});

$('unregister-hub').addEventListener('click', async () => {
  note('hub-admin-msg', '');
  const sure = await confirmDialog(
    `Unregister the hub ${info.name}? Every site that joined it leaves it and stops showing its bar. Nobody's access changes.`,
    'Unregister hub',
  );
  if (!sure) return;
  const result = await api('DELETE', `/sites/${seg(siteId)}/hub`);
  if (!result.ok) return note('hub-admin-msg', messageFor(result.error));
  info = result.data;
  hubEditor = null;
  await redrawChrome();
  await showHubAdmin();
  if (manageSite) await showHub();
  note('hub-admin-msg', 'The hub was unregistered.', true);
});

// ---- save as template, for administrators ----

$('save-template').addEventListener('submit', async (event) => {
  event.preventDefault();
  note('template-msg', '');
  $('template-cleaned').hidden = true;
  const result = await api('POST', `/sites/${seg(siteId)}/template`, {
    name: $('template-name').value.trim(),
    description: $('template-description').value,
  });
  if (!result.ok) return note('template-msg', templateRefusal(result));
  const cleaned = result.data.cleaned ?? [];
  note(
    'template-msg',
    cleaned.length
      ? `The template ${result.data.name} was saved. Some names were not allowed and were changed in it:`
      : `The template ${result.data.name} was saved.`,
    true,
  );
  $('template-cleaned').replaceChildren(...cleaned.map((change) => h('li', {}, `${change.from} → ${change.to}`)));
  $('template-cleaned').hidden = cleaned.length === 0;
  $('template-name').value = '';
  $('template-description').value = '';
});

function fail(text) {
  showMessage(error, text);
  throw new Error(text);
}
