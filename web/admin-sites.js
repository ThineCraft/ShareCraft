// The administrators' Sites page: every site with its state, storage, owners and hub, flagging
// sites with no enabled owner. Archive, Reactivate and Delete for live sites; Restore and
// Delete permanently for deleted ones, each asked first where it takes something away. A link
// to the Brand centre for administrators and brand managers. Live sites can be selected, and
// "Apply a look…" applies one look to them all: another site's, a template's, or a brand-centre
// theme and font package (which changes only the theme and fonts), previewed first, then
// applied in rounds with a progress list of per-site results and a summary toast.

import {
  adminSiteRow,
  api,
  archiveSiteText,
  confirmDialog,
  deleteSiteText,
  h,
  messageFor,
  organisationChrome,
  purgeSiteText,
  requireMe,
  setupTopbar,
  seg,
  showMessage,
} from './common.js';
import { listFilter, mountAdminShell } from './admin-shell.js';
import { lookCardElement } from './look-card-view.js';
import {
  BULK_LOOK_WARNINGS,
  applyChunks,
  brandBulkSource,
  brandLookCard,
  bulkApplySummary,
  effectiveLookCard,
  templateLookCard,
} from './look-cards.js';
import { showToast } from './toast.js';

const $ = (id) => document.getElementById(id);
const error = $('error');
const status = $('status');

let sites = [];
const selected = new Set(); // the ids of the live sites selected

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

/** A site's row with a checkbox to select it (live sites only: a deleted site has no look to change). */
function siteRow(site) {
  const row = adminSiteRow(site, act);
  if (site.state === 'deleted') return row;
  const box = h('input', { type: 'checkbox', class: 'select-site', 'aria-label': `Select ${site.name}` });
  box.checked = selected.has(site.id);
  box.addEventListener('change', () => {
    if (box.checked) selected.add(site.id);
    else selected.delete(site.id);
    showSelection();
  });
  row.prepend(box);
  return row;
}

/** The Apply a look button: on with a selection, else off saying why. */
function showSelection() {
  const count = selected.size;
  $('apply-look').disabled = count === 0;
  $('clear-selection').hidden = count === 0;
  $('selected-count').textContent = count === 0 ? 'Select sites to apply a look to them.' : count === 1 ? '1 site selected.' : `${count} sites selected.`;
}

$('clear-selection').addEventListener('click', () => {
  selected.clear();
  for (const box of document.querySelectorAll('#sites .select-site')) box.checked = false;
  showSelection();
});

async function showSites() {
  const result = await api('GET', '/admin/sites');
  if (!result.ok) return showMessage(error, messageFor(result.error));
  sites = result.data;
  // A site deleted since it was selected is no longer selected.
  for (const id of [...selected]) if (!sites.some((site) => site.id === id && site.state !== 'deleted')) selected.delete(id);
  filter.set(sites.map((site) => ({ row: site, element: siteRow(site) })));
  showSelection();
  const ownerless = sites.filter((site) => site.state !== 'deleted' && site.owners === 0).length;
  $('no-owner').hidden = ownerless === 0;
  $('no-owner').textContent =
    ownerless === 1
      ? 'One site has no enabled owner. Nobody but administrators can manage it.'
      : `${ownerless} sites have no enabled owner. Nobody but administrators can manage them.`;
}

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

// ---- applying a look to the selected sites ----

/** How many sites one request applies a look to: small rounds, so the progress list fills in. */
const ROUND = 10;

let brandCentre = null; // GET /api/brand, once read
let templates = null; // the saved templates, once read
const templateLooks = new Map(); // template id -> its look (null: none), from its package
let busy = false;

const sourceKind = () => document.querySelector('input[name=look-source]:checked')?.value ?? 'site';
const nameOf = (id) => sites.find((site) => site.id === id)?.name ?? 'A site';

async function readChoices() {
  if (!brandCentre) {
    const answer = brand ?? (await api('GET', '/brand'));
    brandCentre = answer.ok ? answer.data : { themes: [], font_packages: [], fonts: [] };
  }
  if (!templates) {
    const answer = await api('GET', '/templates');
    templates = answer.ok ? answer.data.filter((t) => !t.built_in) : [];
  }
}

function fillChoices() {
  const live = sites.filter((site) => site.state !== 'deleted').sort((a, b) => a.name.localeCompare(b.name));
  $('source-site').replaceChildren(...live.map((site) => h('option', { value: site.id }, site.name)));
  $('source-template').replaceChildren(...templates.map((t) => h('option', { value: t.id }, t.name)));
  $('source-template').disabled = templates.length === 0;
  $('template-why').textContent = templates.length ? '' : 'There are no saved templates. The built-in templates carry no look.';
  $('source-theme').replaceChildren(...(brandCentre.themes ?? []).map((t) => h('option', { value: t.from }, t.builtin ? `${t.name} (built in)` : t.name)));
  $('source-package').replaceChildren(...(brandCentre.font_packages ?? []).map((p) => h('option', { value: p.from }, p.builtin ? `${p.name} (built in)` : p.name)));
}

/** What the request sends as `source`, and the warning for it. */
function chosenSource() {
  const kind = sourceKind();
  if (kind === 'site') return { kind, source: { from_site: $('source-site').value }, ready: Boolean($('source-site').value) };
  if (kind === 'template') {
    const id = $('source-template').value;
    return { kind, source: { template: id }, ready: Boolean(id) && Boolean(templateLooks.get(id)) };
  }
  const theme = $('source-theme').value;
  return { kind, source: brandBulkSource(theme, $('source-package').value), ready: Boolean(theme) };
}

const WARNINGS = BULK_LOOK_WARNINGS;

/** The preview of the chosen look, and whether Apply is on (saying why when it is not). */
async function showChoice() {
  const kind = sourceKind();
  $('from-site').hidden = kind !== 'site';
  $('from-template').hidden = kind !== 'template';
  $('from-brand').hidden = kind !== 'brand';
  $('look-warning').textContent = WARNINGS[kind];
  let card = null;
  let why = '';
  if (kind === 'site') {
    const id = $('source-site').value;
    if (id) {
      const answer = await api('GET', `/sites/${seg(id)}/look`);
      card = answer.ok ? effectiveLookCard(answer.data, nameOf(id)) : null;
      if (!answer.ok) why = messageFor(answer.error);
    } else why = 'Choose a site.';
  } else if (kind === 'template') {
    const id = $('source-template').value;
    if (id && !templateLooks.has(id)) {
      const answer = await api('GET', `/templates/${seg(id)}/package`);
      templateLooks.set(id, answer.ok ? (answer.data?.body?.look ?? null) : null);
    }
    card = id ? templateLookCard(templateLooks.get(id), brandCentre.fonts ?? []) : null;
    if (!id) why = 'Choose a template.';
    else if (!card) why = 'That template carries no look of its own. Choose another source.';
  } else {
    const theme = (brandCentre.themes ?? []).find((t) => t.from === $('source-theme').value);
    const pkg = (brandCentre.font_packages ?? []).find((p) => p.from === $('source-package').value);
    card = brandLookCard(theme, pkg && pkg.from !== 'builtin:system' ? pkg : null);
    if (!theme) why = 'Choose a theme.';
  }
  $('look-preview').replaceChildren(card ? lookCardElement(card) : h('p', { class: 'meta' }, 'No preview.'));
  const ready = chosenSource().ready && !why;
  $('look-apply').disabled = busy || !ready;
  $('look-apply-why').textContent = busy ? 'Applying…' : why;
}

for (const input of document.querySelectorAll('input[name=look-source]')) input.addEventListener('change', showChoice);
for (const id of ['source-site', 'source-template', 'source-theme', 'source-package']) $(id).addEventListener('change', showChoice);

$('apply-look').addEventListener('click', async () => {
  if (selected.size === 0) return;
  showMessage($('look-msg'), '');
  $('progress-heading').hidden = true;
  $('apply-progress').hidden = true;
  $('apply-progress').replaceChildren();
  $('look-close').textContent = 'Cancel';
  $('look-for').textContent = selected.size === 1 ? `For 1 selected site: ${nameOf([...selected][0])}.` : `For ${selected.size} selected sites.`;
  $('look-dialog').showModal();
  await readChoices();
  fillChoices();
  await showChoice();
});

$('look-close').addEventListener('click', () => $('look-dialog').close());
$('look-dialog').addEventListener('cancel', (event) => {
  // The dialog stays while a look is being applied, so the progress is not lost.
  if (busy) event.preventDefault();
});

/** One row of the progress list. */
function progressRow(row) {
  return h('li', { class: row.ok ? 'done' : row.pending ? 'pending' : 'failed' }, h('span', { class: 'grow' }, row.name, h('br'), h('small', {}, row.text)));
}

$('look-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  const { source, ready } = chosenSource();
  if (!ready || busy || selected.size === 0) return;
  const ids = [...selected];
  const sure = await confirmDialog(
    `${WARNINGS[sourceKind()]} Apply it to ${ids.length === 1 ? '1 site' : `${ids.length} sites`}? Each site’s history keeps its earlier look, so a site owner can restore it.`,
    'Apply look',
  );
  if (!sure) return;
  busy = true;
  $('look-apply').disabled = true;
  $('look-close').disabled = true;
  $('look-apply-why').textContent = 'Applying…';
  showMessage($('look-msg'), '');
  const names = new Map(ids.map((id) => [id, nameOf(id)]));
  const results = [];
  const draw = () => {
    const done = bulkApplySummary(results, names).rows;
    const waiting = ids.slice(results.length).map((id) => ({ name: names.get(id), ok: false, pending: true, text: 'Waiting…' }));
    $('apply-progress').replaceChildren(...done.map(progressRow), ...waiting.map(progressRow));
  };
  $('progress-heading').hidden = false;
  $('apply-progress').hidden = false;
  draw();
  for (const round of applyChunks(ids, ROUND)) {
    const answer = await api('POST', '/admin/looks/apply', { sites: round, source });
    if (answer.ok && Array.isArray(answer.data?.results)) results.push(...answer.data.results);
    else results.push(...round.map((site) => ({ site, ok: false, error: answer.error ?? 'unavailable' })));
    draw();
  }
  busy = false;
  const summary = bulkApplySummary(results, names);
  showToast(summary.text, { kind: summary.kind });
  $('look-close').disabled = false;
  $('look-close').textContent = 'Close';
  $('look-apply-why').textContent = 'Done. Close this to go back to the sites.';
  $('look-apply').disabled = true;
  selected.clear();
  await showSites();
});
