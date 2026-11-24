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
import { mountAdminShell } from './admin-shell.js';
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
mountAdminShell({ me, current: 'settings' });
await organisationChrome();

let globalEditor = null;
let regional = null;
let scanner = null; // the scanner settings as the server last answered them

// ---- S3 storage state and wording (declared before the first call that can reach it) ----

let s3Answer = null; // the S3 settings as the server last answered them: endpoints and in_use

/** The sentence for each reason the server gives for an endpoint, in plain words. */
const S3_REASON = {
  empty: 'Fill in this field.',
  not_https: 'Use an https address.',
  has_path: 'Give the address without a path.',
  has_query: 'Give the address without a query.',
  has_credentials: 'Leave user names and passwords out of the address.',
  bad_host: 'The address needs a host name.',
  address_literal: 'Use a host name, not an IP address.',
  bad_port: 'The port is not one S3 storage uses.',
  bad_region: 'The region is not valid.',
  bad_name: 'The name is not valid.',
  name_taken: 'Another storage already has this name.',
  too_many: 'There are too many storage entries.',
  bad_range: 'A private range is not valid. Write it like 10.0.0.0/8.',
  reserved: 'That range cannot be allowed.',
  unknown_field: 'That is not a field ShareCraft knows.',
  bad_path_style: 'Choose path style or virtual-hosted style.',
};
/** The field each entry reason is shown beside. */
const S3_FIELD = {
  bad_name: 'name',
  name_taken: 'name',
  not_https: 'url',
  has_path: 'url',
  has_query: 'url',
  has_credentials: 'url',
  bad_host: 'url',
  address_literal: 'url',
  bad_port: 'url',
  bad_region: 'region',
  bad_path_style: 'style',
  bad_range: 'allow',
  reserved: 'allow',
};
const S3_FIELDS = ['name', 'url', 'region', 'style', 'allow'];


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
  await showS3Storage();
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

// ---- S3 storage ----



function clearS3Errors() {
  for (const field of S3_FIELDS) showMessage($(`s3-err-${field}`), '');
}

function showS3Rows(answer) {
  const rows = answer.endpoints.map((endpoint, index) =>
    h(
      'tr',
      {},
      h('th', { scope: 'row' }, endpoint.name),
      h('td', {}, endpoint.url),
      h('td', {}, endpoint.region),
      h('td', {}, endpoint.path_style ? 'Path style' : 'Virtual-hosted style'),
      h('td', {}, endpoint.allow.length > 0 ? endpoint.allow.join(', ') : 'None'),
      h('td', {}, String(answer.in_use?.[endpoint.name] ?? 0)),
      h(
        'td',
        {},
        h('button', { type: 'button', class: 'plain', 'data-index': String(index), 'aria-label': `Remove ${endpoint.name}` }, 'Remove'),
      ),
    ),
  );
  $('s3-rows').replaceChildren(...rows);
  $('s3-none').hidden = answer.endpoints.length > 0;
}

async function showS3Storage() {
  const result = await api('GET', '/settings/s3');
  if (!result.ok) return showMessage(error, messageFor(result.error));
  s3Answer = result.data;
  showS3Rows(s3Answer);
  $('s3').hidden = false;
}

/** A refused endpoint: its reason beside the field it concerns, or in the form's message. */
function s3Refused(result, newIndex) {
  if (result.error !== 'invalid_s3_endpoint') return noteIn('s3-msg', messageFor(result.error));
  const { entry, reason } = result.data ?? {};
  const text = S3_REASON[reason] ?? 'The storage was not saved. Check the entry and try again.';
  const field = entry === newIndex ? S3_FIELD[reason] : undefined;
  if (field) {
    showMessage($(`s3-err-${field}`), text);
    return $(`s3-${field}`).focus();
  }
  const where = typeof entry === 'number' && entry !== newIndex ? `Storage ${entry + 1}: ` : '';
  return noteIn('s3-msg', `${where}${text}`);
}

/** Saves the whole list of endpoints the server keeps; `newIndex` names the entry being added. */
async function sendS3(endpoints, newIndex, done) {
  const result = await api('PUT', '/settings/s3', { endpoints });
  if (!result.ok) return s3Refused(result, newIndex);
  s3Answer = result.data;
  showS3Rows(s3Answer);
  noteIn('s3-msg', done, true);
}

$('s3-add').addEventListener('submit', async (event) => {
  event.preventDefault();
  if (!s3Answer) return;
  clearS3Errors();
  noteIn('s3-msg', '');
  const entry = {
    name: $('s3-name').value.trim(),
    url: $('s3-url').value.trim(),
    region: $('s3-region').value.trim(),
    path_style: $('s3-style').value === 'path',
    allow: $('s3-allow')
      .value.split(',')
      .map((range) => range.trim())
      .filter((range) => range !== ''),
  };
  const endpoints = [...s3Answer.endpoints, entry];
  await sendS3(endpoints, s3Answer.endpoints.length, 'Added. Libraries can connect to it now.');
  if (s3Answer.endpoints.length === endpoints.length) {
    $('s3-name').value = '';
    $('s3-url').value = '';
    $('s3-region').value = '';
    $('s3-style').value = 'path';
    $('s3-allow').value = '';
  }
});

$('s3-rows').addEventListener('click', async (event) => {
  const button = event.target.closest('button[data-index]');
  if (!button || !s3Answer) return;
  const index = Number(button.dataset.index);
  const endpoint = s3Answer.endpoints[index];
  if (!endpoint) return;
  const users = s3Answer.in_use?.[endpoint.name] ?? 0;
  const sure = await confirmDialog(
    users > 0
      ? `Remove ${endpoint.name}? ${users} ${users === 1 ? 'library reads' : 'libraries read'} from it, and their reads stop at once.`
      : `Remove ${endpoint.name}?`,
    'Remove storage',
  );
  if (!sure) return button.focus();
  noteIn('s3-msg', '');
  await sendS3(
    s3Answer.endpoints.filter((_, position) => position !== index),
    -1,
    `Removed ${endpoint.name}.`,
  );
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
