// The administrators' Sites page: every site with its state, storage, owners and hub, flagging
// sites with no enabled owner. Archive, Reactivate and Delete for live sites; Restore and
// Delete permanently for deleted ones, each asked first where it takes something away. A link
// to the Brand centre for administrators and brand managers.

import {
  adminSiteRow,
  api,
  archiveSiteText,
  confirmDialog,
  deleteSiteText,
  messageFor,
  organisationChrome,
  purgeSiteText,
  requireMe,
  setupTopbar,
  seg,
  showMessage,
} from './common.js';
import { listFilter, mountAdminShell } from './admin-shell.js';

const $ = (id) => document.getElementById(id);
const error = $('error');
const status = $('status');

let me;
try {
  me = await requireMe();
} catch (err) {
  status.textContent = err.message;
  throw err;
}
setupTopbar(me, error);
await organisationChrome();

// The brand centre is looked after by administrators and the brand managers they name.
const brand = me.is_admin ? null : await api('GET', '/brand');
mountAdminShell({ me, current: 'sites', brandManager: me.is_admin || Boolean(brand?.data?.can_manage) });

const STATE_WORDS = { active: 'Active live', archived: 'Archived', deleted: 'Deleted' };
const filter = listFilter({
  id: 'find-site',
  label: 'Find a site by name, state or hub',
  nouns: ['site', 'sites'],
  fields: ['name', (site) => STATE_WORDS[site.state] ?? site.state, (site) => (site.kind === 'communication' ? 'Communication' : 'Team'), 'hub.name', (site) => (site.is_hub ? 'Hub' : '')],
  list: $('sites'),
  empty: 'There are no sites yet.',
});
$('sites').before(filter.element);

if (!me.is_admin) {
  status.textContent = brand?.data?.can_manage
    ? 'Only administrators can manage sites. As a brand manager you can open the Brand centre.'
    : 'Only administrators can manage sites.';
} else {
  $('content').hidden = false;
  await showSites();
}

async function showSites() {
  const result = await api('GET', '/admin/sites');
  if (!result.ok) return showMessage(error, messageFor(result.error));
  const sites = result.data;
  filter.set(sites.map((site) => ({ row: site, element: adminSiteRow(site, act) })));
  const ownerless = sites.filter((site) => site.state !== 'deleted' && site.owners === 0).length;
  $('no-owner').hidden = ownerless === 0;
  $('no-owner').textContent =
    ownerless === 1
      ? 'One site has no enabled owner. Nobody but administrators can manage it.'
      : `${ownerless} sites have no enabled owner. Nobody but administrators can manage them.`;
}

/** What each action asks first (null: nothing is taken away), sends, and says when done. */
const ACTIONS = {
  archive: {
    ask: (site) => [archiveSiteText(site), 'Archive'],
    send: (site) => api('POST', `/sites/${seg(site.id)}/archive`),
    done: (site) => `${site.name} is archived.`,
  },
  reactivate: {
    ask: null,
    send: (site) => api('POST', `/sites/${seg(site.id)}/reactivate`),
    done: (site) => `${site.name} is active again.`,
  },
  delete: {
    ask: (site) => [deleteSiteText(site), 'Delete site'],
    send: (site) => api('DELETE', `/sites/${seg(site.id)}`),
    done: (site) => `${site.name} is deleted. It can be restored for 93 days.`,
  },
  restore: {
    ask: null,
    send: (site) => api('POST', `/admin/sites/${seg(site.id)}/restore`),
    done: (site, data) =>
      data?.renamed_from ? `${data.renamed_from} is restored as ${data.name}, since its name was taken.` : `${site.name} is restored.`,
  },
  purge: {
    ask: (site) => [purgeSiteText(site), 'Delete permanently'],
    send: (site) => api('DELETE', `/admin/sites/${seg(site.id)}`),
    done: (site) => `${site.name} was deleted permanently.`,
  },
};

async function act(action, site) {
  showMessage(error, '');
  const step = ACTIONS[action];
  if (!step) return;
  if (step.ask && !(await confirmDialog(...step.ask(site)))) return;
  const result = await step.send(site);
  if (!result.ok) return showMessage(error, messageFor(result.error));
  status.textContent = step.done(site, result.data);
  await showSites();
}
