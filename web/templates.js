// Site templates, for administrators: the built-in Team and Communication templates and the
// saved ones, each with the look it carries (its theme's colours and a sample of its fonts) and
// downloadable as a package; importing a package (refused with its reason, or accepted with the
// names that had to change and the brand-centre references its look could not keep); deleting
// a saved template.

import {
  api,
  confirmDialog,
  h,
  kindLabel,
  messageFor,
  organisationChrome,
  requireMe,
  setupTopbar,
  seg,
  showMessage,
  templateReasonText,
  templateRefusal,
} from './common.js';
import { listFilter, mountAdminShell } from './admin-shell.js';
import { lookCardElement } from './look-card-view.js';
import { templateLookCard, templateLookReport } from './look-cards.js';

const $ = (id) => document.getElementById(id);
const error = $('error');
const status = $('status');
/** The largest package the server takes, in bytes. */
const MAX_PACKAGE = 1024 * 1024;
let brandFonts = null; // GET /api/brand's fonts, once read
const looks = new Map(); // template id -> its look card (null: none), from its package

let me;
try {
  me = await requireMe();
} catch (err) {
  status.textContent = err.message;
  throw err;
}
setupTopbar(me, error);
mountAdminShell({ me, current: 'templates' });
await organisationChrome();

const filter = listFilter({
  id: 'find-template',
  label: 'Find a template',
  nouns: ['template', 'templates'],
  fields: ['name', 'description', (template) => kindLabel(template.kind), (template) => (template.built_in ? 'Built in' : 'Saved')],
  list: $('templates'),
  empty: 'There are no templates yet. Import a package below.',
});
$('templates').before(filter.element);

if (!me.is_admin) {
  status.textContent = 'Only administrators can manage templates.';
} else {
  $('content').hidden = false;
  await showTemplates();
}

/** Shows `text` under the import form; `ok` for a confirmation. */
function note(text, ok = false) {
  $('import-msg').classList.toggle('ok', ok);
  showMessage($('import-msg'), text);
}

async function showTemplates() {
  const [result, brand] = await Promise.all([api('GET', '/templates'), brandFonts ? null : api('GET', '/brand')]);
  if (!result.ok) return showMessage(error, messageFor(result.error));
  if (!brandFonts) brandFonts = brand?.ok && Array.isArray(brand.data.fonts) ? brand.data.fonts : [];
  filter.set(result.data.map((template) => ({ row: template, element: templateRow(template) })));
  // Each saved template's look arrives from its package; the built-ins carry none.
  await Promise.all(
    result.data
      .filter((template) => !template.built_in && !looks.has(template.id))
      .map(async (template) => {
        const answer = await api('GET', `/templates/${seg(template.id)}/package`);
        looks.set(template.id, answer.ok ? templateLookCard(answer.data?.body?.look ?? null, brandFonts) : null);
        const box = document.querySelector(`[data-template-look="${CSS.escape(template.id)}"]`);
        if (box) box.replaceChildren(lookOf(template));
      }),
  );
}

/** The look a template carries, as a card; words for one without a look. */
function lookOf(template) {
  if (template.built_in) return h('small', { class: 'meta' }, 'Look: the organisation’s default');
  if (!looks.has(template.id)) return h('small', { class: 'meta' }, 'Reading its look…');
  const card = looks.get(template.id);
  return card ? lookCardElement(card) : h('small', { class: 'meta' }, 'Look: the organisation’s default');
}

/** One template: name, kind, description, Download package and, for a saved one, Delete. Text only. */
function templateRow(template) {
  const download = h(
    'a',
    {
      class: 'button plain',
      href: `/api/templates/${seg(template.id)}/package`,
      download: '',
      'aria-label': `Download the package of ${template.name}`,
    },
    'Download package',
  );
  let remove = null;
  if (!template.built_in) {
    remove = h('button', { type: 'button', class: 'plain', 'aria-label': `Delete the template ${template.name}` }, 'Delete');
    remove.addEventListener('click', () => deleteTemplate(template));
  }
  return h(
    'li',
    {},
    h(
      'span',
      { class: 'grow' },
      h('strong', {}, template.name),
      h('br'),
      h('small', {}, template.built_in ? `Built in · makes a ${kindLabel(template.kind)}` : `Saved · makes a ${kindLabel(template.kind)}`),
      template.description ? h('br') : null,
      template.description ? h('span', { class: 'description' }, template.description) : null,
      h('span', { class: 'template-look', 'data-template-look': template.id }, lookOf(template)),
    ),
    h('span', { class: 'actions' }, download, remove),
  );
}

async function deleteTemplate(template) {
  showMessage(error, '');
  const sure = await confirmDialog(
    `Delete the template ${template.name}? It can no longer be used to make sites. Sites already made from it keep everything they have.`,
    'Delete template',
  );
  if (!sure) return;
  const result = await api('DELETE', `/templates/${seg(template.id)}`);
  if (!result.ok) return showMessage(error, messageFor(result.error));
  status.textContent = `The template ${template.name} was deleted.`;
  await showTemplates();
}

$('import').addEventListener('submit', async (event) => {
  event.preventDefault();
  note('');
  $('import-cleaned').hidden = true;
  const file = $('package').files?.[0];
  if (!file) return note('Choose a package file first.');
  if (file.size > MAX_PACKAGE) return note(templateReasonText('too_large'));
  let text;
  try {
    text = await file.text();
  } catch {
    return note('That file could not be read.');
  }
  const result = await api('POST', '/templates/import', text, { raw: true });
  if (!result.ok) return note(templateRefusal(result));
  const cleaned = result.data.cleaned ?? [];
  const lookReport = templateLookReport({ unresolved: result.data.unresolved ?? [] });
  note(
    [
      `The template ${result.data.name} was imported.`,
      lookReport,
      cleaned.length ? 'Some names were not allowed and were changed:' : '',
    ]
      .filter(Boolean)
      .join(' '),
    true,
  );
  $('import-cleaned').replaceChildren(...cleaned.map((change) => h('li', {}, `${change.from} → ${change.to}`)));
  $('import-cleaned').hidden = cleaned.length === 0;
  $('package').value = '';
  await showTemplates();
});
