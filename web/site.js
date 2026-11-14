import {
  api,
  h,
  messageFor,
  offerRequest,
  pageFor,
  queryId,
  can,
  requireMe,
  roleLine,
  seg,
  setupTopbar,
  showCrumbs,
  showMessage,
} from './common.js';

const $ = (id) => document.getElementById(id);
const error = $('error');

const me = await requireMe();
setupTopbar(me, error);
const siteId = queryId(location.search);
const MISSING = 'This site does not exist, or you do not have access to it.';
if (!siteId) fail(MISSING);

const here = await showCrumbs($('crumbs'), siteId, { linkLast: false });
if (!here) {
  offerRequest(error, siteId);
  fail(MISSING);
}
const site = here.path[here.path.length - 1];
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

await showLibraries();
if (manageSite) await showStorage();

/** For people who manage the site: what it holds against its quota, warning from 90%. */
async function showStorage() {
  const result = await api('GET', `/sites/${seg(siteId)}`);
  if (!result.ok || result.data.used === undefined) return;
  const { used, quota, quota_warning: warning } = result.data;
  const box = $('storage');
  box.textContent =
    quota === null
      ? `This site holds ${bytes(used)}. It has no storage quota.`
      : `This site holds ${bytes(used)} of its ${bytes(quota)} storage quota.` +
        (warning ? ' It is nearly full: delete or purge what is not needed, or ask an administrator for more.' : '');
  box.classList.toggle('msg', Boolean(warning));
  box.hidden = false;
}

/** `n` bytes in the largest unit that keeps it at 1 or more. */
function bytes(n) {
  const units = ['bytes', 'KB', 'MB', 'GB', 'TB'];
  let value = n;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return unit === 0 ? `${value} bytes` : `${value.toFixed(1)} ${units[unit]}`;
}

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
