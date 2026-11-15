// The organisation's settings, for administrators: whether links for anyone may be made and
// used and how long one may last; the default language and time zone; and the global
// navigation shown on every page. The server decides; refusals are shown as sentences.

import {
  api,
  confirmDialog,
  fillSelect,
  h,
  megabytesText,
  parseMegabytes,
  parseTypeList,
  typeListText,
  languageChoices,
  messageFor,
  organisationChrome,
  requireMe,
  setupTopbar,
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
setupTopbar(me, error);
await organisationChrome();

let globalEditor = null;
let regional = null;
let scanner = null; // the scanner settings as the server last answered them


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
  await showFiles();
  await showScanner();
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

// ---- file types blocked everywhere ----

function showBlocked(types) {
  $('blocked-types').value = typeListText(types);
  $('blocked-list').replaceChildren(...types.map((type) => h('li', {}, type)));
  $('blocked-list').hidden = types.length === 0;
}

async function showFiles() {
  const result = await api('GET', '/settings/files');
  if (!result.ok) return noteIn('files-msg', messageFor(result.error));
  showBlocked(result.data.blocked_file_types);
  $('files').hidden = false;
}

$('files-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  noteIn('files-msg', '');
  const typed = parseTypeList($('blocked-types').value);
  if (typed.error) return noteIn('files-msg', typed.error);
  const result = await api('PUT', '/settings/files', { blocked_file_types: typed.types });
  if (!result.ok) return noteIn('files-msg', messageFor(result.error));
  showBlocked(result.data.blocked_file_types);
  noteIn('files-msg', 'Saved. It applies to every new upload, copy, move and rename at once.', true);
});

// ---- the malware scanner ----

function showScannerSettings(settings) {
  scanner = settings;
  $('scanner-url').value = settings.scanner_url;
  $('scanner-timeout').value = String(settings.scanner_timeout_seconds);
  $('scanner-max').value = megabytesText(settings.scanner_max_bytes);
  $('scanner-unavailable').value = settings.scanner_unavailable;
  $('scanner-too-large').value = settings.scanner_too_large;
}

async function showScanner() {
  const result = await api('GET', '/settings/scanner');
  if (!result.ok) return noteIn('scanner-msg', messageFor(result.error));
  showScannerSettings(result.data);
  $('scanner').hidden = false;
}

/** What the server said when it asked the scanner (the settings are kept either way). */
function testAnswer(answer) {
  if (!answer.ok) {
    return { text: `Saved, but the scanner did not answer: ${answer.error}. New files wait until it does.`, ok: false };
  }
  if (!answer.settings?.scanner_url) return { text: 'Saved. There is no scanner, so new files are available at once.', ok: true };
  return { text: `Saved. The scanner answered${answer.server ? ` (${answer.server})` : ''}.`, ok: true };
}

/** Sends `change` and shows the scanner's answer. */
async function sendScanner(change) {
  noteIn('scanner-msg', 'Asking the scanner…', true);
  const result = await api('PUT', '/settings/scanner', change);
  if (!result.ok) return noteIn('scanner-msg', messageFor(result.error));
  if (result.data.settings) showScannerSettings(result.data.settings);
  const told = testAnswer(result.data);
  noteIn('scanner-msg', told.text, told.ok);
}

$('scanner-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  const typed = $('scanner-timeout').value.trim();
  const seconds = /^[0-9]{1,4}$/.test(typed) ? Number(typed) : 0;
  if (seconds < 5 || seconds > 3600) return noteIn('scanner-msg', 'Enter the wait as a whole number of seconds from 5 to 3600.');
  const largest = parseMegabytes($('scanner-max').value);
  if (largest.error || largest.bytes === null) {
    return noteIn('scanner-msg', 'Enter the largest file the scanner takes as a number of megabytes.');
  }
  const wanted = {
    scanner_url: $('scanner-url').value.trim(),
    scanner_timeout_seconds: seconds,
    scanner_max_bytes: largest.bytes,
    scanner_unavailable: $('scanner-unavailable').value,
    scanner_too_large: $('scanner-too-large').value,
  };
  const change = {};
  for (const [key, value] of Object.entries(wanted)) if (value !== scanner?.[key]) change[key] = value;
  if (Object.keys(change).length === 0) return noteIn('scanner-msg', 'Nothing has changed.', true);
  await sendScanner(change);
});

// Test asks the scanner again at the address as saved; nothing typed but unsaved is sent.
$('scanner-test').addEventListener('click', async () => {
  if (!scanner) return;
  if (!scanner.scanner_url) return noteIn('scanner-msg', 'There is no scanner to test. Enter its address and save first.');
  await sendScanner({ scanner_url: scanner.scanner_url });
});

$('scan-existing').addEventListener('click', async () => {
  noteIn('scan-existing-msg', '', true);
  const sure = await confirmDialog(
    'Send every file stored before scanning to the scanner? They stay available while they wait; any it finds a threat in are quarantined.',
    'Scan existing files',
  );
  if (!sure) return;
  const result = await api('POST', '/admin/scan-existing');
  if (!result.ok) return noteIn('scan-existing-msg', messageFor(result.error));
  const queued = result.data?.queued ?? 0;
  noteIn('scan-existing-msg', `${queued} ${queued === 1 ? 'file was' : 'files were'} queued for the scanner.`, true);
});
