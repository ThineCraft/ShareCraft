// The organisation's settings, for administrators: whether links for anyone may be made and
// used and how long one may last; the default language and time zone; and the global
// navigation shown on every page. The server decides; refusals are shown as sentences.

import {
  api,
  fillSelect,
  languageChoices,
  messageFor,
  organisationChrome,
  requireMe,
  showMessage,
  timeZoneChoices,
} from './common.js';
import { navigationBody, navigationEditor, placePicker } from './navigation.js';

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
$('who').textContent = `Signed in as ${me.name}`;
await organisationChrome();

let globalEditor = null;
let regional = null;


if (!me.is_admin) {
  status.textContent = 'Only administrators can change the organisation settings.';
} else {
  const result = await api('GET', '/settings/sharing');
  if (result.ok) {
    show(result.data);
    $('settings').hidden = false;
    $('sharing-heading').hidden = false;
  } else {
    showMessage(error, messageFor(result.error));
  }
  await showRegional();
  await showGlobal();
}

function show(settings) {
  $('anyone-links').checked = settings.anyone_links;
  $('max-days').value = String(settings.anyone_link_max_days);
}

function note(text, isError) {
  error.classList.toggle('ok', !isError);
  showMessage(error, text);
}

/** Shows `text` in the message paragraph `id`; `ok` for a confirmation rather than a refusal. */
function noteIn(id, text, ok = false) {
  $(id).classList.toggle('ok', ok);
  showMessage($(id), text);
}

$('settings').addEventListener('submit', async (event) => {
  event.preventDefault();
  const typed = $('max-days').value.trim();
  const days = /^[0-9]{1,3}$/.test(typed) ? Number(typed) : 0;
  if (days < 1 || days > 365) return note('Enter a whole number of days from 1 to 365.', true);
  const result = await api('PUT', '/settings/sharing', {
    anyone_links: $('anyone-links').checked,
    anyone_link_max_days: days,
  });
  if (result.ok) show(result.data);
  note(result.ok ? 'Saved.' : messageFor(result.error), !result.ok);
});

// ---- the organisation's default language and time zone ----

function showDefaults(defaults) {
  regional = defaults;
  fillSelect($('default-language'), languageChoices(defaults.default_language), defaults.default_language);
  fillSelect($('default-zone'), timeZoneChoices(defaults.default_time_zone), defaults.default_time_zone);
}

async function showRegional() {
  const result = await api('GET', '/settings/regional');
  if (!result.ok) return noteIn('regional-msg', messageFor(result.error));
  showDefaults(result.data);
  $('regional').hidden = false;
}

$('regional-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  noteIn('regional-msg', '');
  const change = {};
  if ($('default-language').value !== regional?.default_language) change.default_language = $('default-language').value;
  if ($('default-zone').value !== regional?.default_time_zone) change.default_time_zone = $('default-zone').value;
  if (Object.keys(change).length === 0) return noteIn('regional-msg', 'Nothing has changed.', true);
  const result = await api('PUT', '/settings/regional', change);
  if (!result.ok) return noteIn('regional-msg', messageFor(result.error));
  showDefaults(result.data);
  await organisationChrome();
  noteIn('regional-msg', 'Saved.', true);
});

// ---- the global navigation ----

/** The sites, for links in the global bar: `[{id, name, kind}]`. */
async function sites() {
  const result = await api('GET', '/sites');
  return result.ok ? result.data.map((site) => ({ id: site.id, name: site.name, kind: 'site' })) : [];
}

async function showGlobal() {
  const [answer, groups] = await Promise.all([api('GET', '/navigation?edit=1'), api('GET', '/groups')]);
  if (!answer.ok) return noteIn('global-msg', messageFor(answer.error));
  globalEditor = navigationEditor({
    items: answer.data.items,
    groups: groups.ok ? groups.data : [],
    picker: placePicker({ first: sites, label: 'Site, library or folder' }),
    label: 'Links of the global navigation',
  });
  $('global-editor').replaceChildren(globalEditor.element);
  $('global').hidden = false;
}

$('save-global').addEventListener('click', async () => {
  noteIn('global-msg', '');
  if (!globalEditor) return;
  const body = navigationBody({ automatic: false, items: globalEditor.items() });
  if (body.error) return noteIn('global-msg', body.error);
  const result = await api('PUT', '/navigation', body);
  if (!result.ok) return noteIn('global-msg', messageFor(result.error));
  await organisationChrome();
  noteIn('global-msg', 'The global navigation was saved.', true);
});
