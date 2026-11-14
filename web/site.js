import {
  api,
  directoryRow,
  formatDate,
  h,
  kindLabel,
  messageFor,
  offerRequest,
  pageFor,
  queryId,
  can,
  confirmDialog,
  quotaSubmit,
  quotaText,
  usagePercent,
  requireMe,
  roleLine,
  seg,
  setupTopbar,
  showCrumbs,
  showMessage,
  siteChrome,
} from './common.js';

const $ = (id) => document.getElementById(id);
const error = $('error');

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
let info = (await siteChrome(siteId, { currentId: siteId })).site;
$('title').textContent = site.name;
document.title = `${site.name} — ShareCraft`;
$('role').textContent = roleLine(here.access);
const manageSite = can(here.access, 'manage_site');
const manageAccess = can(here.access, 'manage_access');
$('new-library').hidden = !manageSite;
$('groups-link').hidden = !manageSite;
$('access-link').hidden = !manageAccess;
$('explorer-link').hidden = !manageAccess;
$('site-tools').hidden = !(manageSite || manageAccess);
$('access-link').href = `/access.html?id=${seg(siteId)}`;
$('explorer-link').href = `/explorer.html?root=${seg(siteId)}`;
$('groups-link').href = `/groups.html?site=${seg(siteId)}`;
$('bin-link').href = `/bin.html?site=${seg(siteId)}`;

$('new-library').addEventListener('submit', async (event) => {
  event.preventDefault();
  showMessage(error, '');
  const result = await api('POST', `/nodes/${seg(siteId)}/children`, {
    kind: 'library',
    name: $('library-name').value.trim(),
  });
  if (result.ok) location.assign(pageFor('library', result.data.id));
  else showMessage(error, messageFor(result.error));
});

await showAbout();
await showLibraries();
if (manageSite) await showStorage(info);
else if (me.is_admin) showQuotaOnly();
await showMembers();

/**
 * What the site is: its kind, description, owners and hub, and the archived banner. People who
 * manage it may join it to a hub they can see, or leave the one it joined; a hub joins none.
 */
async function showAbout() {
  if (!info) return;
  $('about').hidden = false;
  $('kind-label').textContent = kindLabel(info.kind);
  $('hub-mark').hidden = !info.is_hub;
  $('description').textContent = info.description ?? '';
  $('description').hidden = !info.description;
  $('owners').textContent = info.owners?.length ? info.owners.join(', ') : 'No owners';
  if (info.is_hub) $('hub-line').replaceChildren('This site is a hub. Other sites can join it.');
  else if (info.hub) $('hub-line').replaceChildren(h('a', { href: pageFor('site', info.hub.id) }, info.hub.name));
  else $('hub-line').replaceChildren('None');
  $('archived-banner').hidden = !info.archived;
  if (info.archived?.at) {
    $('archived-banner').replaceChildren(
      ARCHIVED,
      ' ',
      h('small', {}, `Archived ${formatDate(info.archived.at)}${info.archived.by ? ` by ${info.archived.by}` : ''}.`),
    );
  }
  const mayJoin = can(info.access, 'manage_site') && !info.is_hub;
  $('leave-hub-row').hidden = !(mayJoin && info.hub);
  $('join-hub').hidden = true;
  if (mayJoin && !info.hub) {
    const hubs = await api('GET', '/hubs');
    const choices = hubs.ok ? hubs.data.filter((hub) => hub.id !== siteId) : [];
    $('hub-choice').replaceChildren(...choices.map((hub) => h('option', { value: hub.id }, hub.name)));
    $('join-hub').hidden = choices.length === 0;
  }
}

/** Joins the hub `hub`, or leaves the one joined (`null`), then draws the site and its bars again. */
async function setHub(hub) {
  showMessage(error, '');
  const result = await api('PUT', `/sites/${seg(siteId)}/joined-hub`, { hub });
  if (!result.ok) return showMessage(error, messageFor(result.error));
  info = (await siteChrome(siteId, { currentId: siteId })).site ?? result.data;
  await showAbout();
}

$('join-hub').addEventListener('submit', async (event) => {
  event.preventDefault();
  if ($('hub-choice').value) await setHub($('hub-choice').value);
});
$('leave-hub').addEventListener('click', () => setHub(null));

/** On a hub's page: the sites that joined it which the person can see. */
async function showMembers() {
  if (!info?.is_hub) return;
  const result = await api('GET', `/hubs/${seg(siteId)}/sites`);
  if (!result.ok) return;
  $('members').hidden = false;
  $('members-empty').hidden = result.data.length > 0;
  $('member-sites').replaceChildren(...result.data.map(directoryRow));
}

/**
 * For people who manage the site: what it holds against its quota, warning from 90%. `known` is
 * the site as already read, if it was.
 */
async function showStorage(known) {
  let data = known;
  if (!data || data.used === undefined) {
    const result = await api('GET', `/sites/${seg(siteId)}`);
    if (!result.ok) return;
    data = result.data;
  }
  if (data.used === undefined) return;
  const { used, quota, quota_warning: warning } = data;
  $('storage').hidden = false;
  $('storage-text').textContent = quotaText(data);
  $('storage-text').classList.toggle('msg', Boolean(warning));
  const percent = usagePercent(used, quota);
  $('storage-bar').hidden = percent === null;
  if (percent !== null) {
    $('storage-bar').value = percent;
    $('storage-bar').textContent = `${percent}%`;
    $('storage-bar').setAttribute('aria-label', `${percent}% of the storage quota used`);
    $('storage-bar').classList.toggle('warn', Boolean(warning));
  }
  $('quota-form').hidden = !me.is_admin;
  $('quota-remove').hidden = !me.is_admin;
  if (me.is_admin) $('quota-gb').value = quota === null ? '' : String(Math.round((quota / 1024 ** 3) * 100) / 100);
}

/**
 * Administrators may set a quota even where they hold no manage_site role here; the server
 * shows usage only to people who manage the site, so they get the field without the figures.
 */
function showQuotaOnly() {
  $('storage').hidden = false;
  $('storage-text').textContent = 'The current quota and usage are shown to people who manage this site.';
  $('quota-form').hidden = false;
  $('quota-remove').hidden = false;
}

/** Sets the quota to `bytes` (null for none) and shows the result. */
async function putQuota(bytes) {
  const result = await api('PUT', `/sites/${seg(siteId)}/quota`, { bytes });
  if (!result.ok) return showMessage(error, messageFor(result.error));
  if (manageSite) await showStorage(null);
  else showMessage(error, bytes === null ? 'The quota was removed.' : 'The quota was set.');
}

$('quota-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  showMessage(error, '');
  const asked = quotaSubmit($('quota-gb').value, { blind: true });
  if (asked.error) return showMessage(error, asked.error);
  await putQuota(asked.bytes);
});

$('quota-remove').addEventListener('click', async () => {
  showMessage(error, '');
  const sure = await confirmDialog('Remove the storage quota of this site? It will then have no limit.', 'Remove quota');
  if (sure) await putQuota(null);
});

async function showLibraries() {
  const result = await api('GET', `/nodes/${seg(siteId)}/children?limit=500`);
  if (!result.ok) {
    showMessage(error, messageFor(result.error));
    return;
  }
  const libraries = result.data.items.filter((item) => item.kind === 'library');
  $('empty').hidden = libraries.length > 0;
  $('libraries').replaceChildren(
    ...libraries.map((library) =>
      h(
        'li',
        {},
        h('span', { class: 'grow' }, h('a', { href: pageFor('library', library.id) }, library.name)),
        library.access ? h('small', {}, roleLine(library.access)) : h('small', { class: 'mark' }, 'Limited access'),
        library.inherits ? null : h('small', { class: 'mark' }, 'Own permissions'),
      ),
    ),
  );
}

function fail(text) {
  showMessage(error, text);
  throw new Error(text);
}
