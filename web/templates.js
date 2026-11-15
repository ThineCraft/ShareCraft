// Site templates, for administrators: the built-in Team and Communication templates and the
// saved ones, each downloadable as a package; importing a package (refused with its reason, or
// accepted with the names that had to change); deleting a saved template.

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

const $ = (id) => document.getElementById(id);
const error = $('error');
const status = $('status');
/** The largest package the server takes, in bytes. */
const MAX_PACKAGE = 1024 * 1024;

let me;
try {
  me = await requireMe();
} catch (err) {
  status.textContent = err.message;
  throw err;
}
setupTopbar(me, error);
await organisationChrome();

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
  const result = await api('GET', '/templates');
  if (!result.ok) return showMessage(error, messageFor(result.error));
  $('templates').replaceChildren(...result.data.map(templateRow));
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
  note(
    cleaned.length
      ? `The template ${result.data.name} was imported. Some names were not allowed and were changed:`
      : `The template ${result.data.name} was imported.`,
    true,
  );
  $('import-cleaned').replaceChildren(...cleaned.map((change) => h('li', {}, `${change.from} → ${change.to}`)));
  $('import-cleaned').hidden = cleaned.length === 0;
  $('package').value = '';
  await showTemplates();
});
