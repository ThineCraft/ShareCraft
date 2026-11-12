import {
  api,
  h,
  messageFor,
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
if (!here) fail(MISSING);
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
