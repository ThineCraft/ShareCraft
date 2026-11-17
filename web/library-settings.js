// A library's kind and settings, for the people who manage its site: description, kind (asset
// only for administrators), folders on or off, the file types it allows and blocks, and the
// largest file it takes. The server decides every change; refusals are shown as sentences.

import {
  api,
  can,
  confirmDialog,
  formatSize,
  h,
  libraryKindChoices,
  libraryKindLabel,
  libraryReasonText,
  megabytesText,
  messageFor,
  pageFor,
  parseMegabytes,
  parseTypeList,
  queryId,
  requireMe,
  seg,
  setupTopbar,
  showCrumbs,
  showMessage,
  siteChrome,
  typeListText,
} from './common.js';
import { webhookInstructions } from './source-panel.js';

const $ = (id) => document.getElementById(id);
const error = $('error');
const MISSING = 'This library does not exist, or you do not have access to it.';
const sourceError = (code) =>
  code === 'invalid_request'
    ? 'Check the branch, folder, poll interval and token, then try again.'
    : messageFor(code);

const me = await requireMe();
setupTopbar(me, error);
const libraryId = queryId(location.search);
if (!libraryId) fail(MISSING);
const here = await showCrumbs($('crumbs'), libraryId);
if (!here) fail(MISSING);
const self = here.path[here.path.length - 1];
if (self.kind !== 'library') {
  location.replace(pageFor(self.kind, self.id));
  await new Promise(() => {});
}
await siteChrome(here.path[0].id, { currentId: libraryId });
if (!can(here.access, 'manage_site')) fail('Only the owners of this site can change the settings of its libraries.');

const first = await api('GET', `/libraries/${seg(libraryId)}`);
if (!first.ok) fail(first.status === 404 ? MISSING : messageFor(first.error));
let library = first.data;
show(library);
$('settings').hidden = false;

let source = null;
const sourceAnswer = await api('GET', `/libraries/${seg(libraryId)}/source`);
if (sourceAnswer.ok) {
  source = sourceAnswer.data;
  showSource();
} else if (sourceAnswer.status !== 404) {
  showMessage(error, messageFor(sourceAnswer.error));
}

function fail(text) {
  showMessage(error, text);
  throw new Error(text);
}

/** Fills the form with `settings` as the server answered them (lists already normalised). */
function show(settings) {
  library = settings;
  $('title').textContent = `Settings of ${settings.name}`;
  document.title = `${settings.name} settings — ShareCraft`;
  $('description').value = settings.description ?? '';
  // Only administrators set the asset kind or change a library away from it.
  const choices = libraryKindChoices(me.is_admin);
  if (!choices.some((choice) => choice.value === settings.kind)) {
    choices.push({ value: settings.kind, label: libraryKindLabel(settings.kind) });
  }
  $('kind').replaceChildren(...choices.map((choice) => h('option', { value: choice.value }, choice.label)));
  $('kind').value = settings.kind;
  $('kind').disabled = settings.kind === 'asset' && !me.is_admin;
  $('folders').checked = settings.folders;
  $('allowed').value = typeListText(settings.allowed_types);
  $('blocked').value = typeListText(settings.blocked_types);
  $('max-mb').value = megabytesText(settings.max_file_bytes);
  showEffective(settings);
}

/** What the library takes in effect, one line each (the kind's own list included). */
function showEffective(settings) {
  const lines = [];
  lines.push(
    Array.isArray(settings.effective_allowed_types)
      ? `Takes only ${typeListText(settings.effective_allowed_types)}`
      : 'Takes every type the organisation allows',
  );
  if (settings.blocked_types?.length) lines.push(`Never takes ${typeListText(settings.blocked_types)}`);
  lines.push(typeof settings.max_file_bytes === 'number' ? `Files up to ${formatSize(settings.max_file_bytes)}` : 'No size limit of its own');
  lines.push(settings.folders ? 'Folders on' : 'Folders off');
  $('effective').replaceChildren(...lines.map((line) => h('li', {}, line)));
}

const sameList = (a, b) => JSON.stringify(a ?? null) === JSON.stringify(b ?? null);

$('settings').addEventListener('submit', async (event) => {
  event.preventDefault();
  showMessage(error, '');
  showMessage($('saved'), '');
  const allowed = parseTypeList($('allowed').value);
  if (allowed.error) return showMessage(error, `Allowed file types: ${allowed.error}`);
  const blocked = parseTypeList($('blocked').value);
  if (blocked.error) return showMessage(error, `Blocked file types: ${blocked.error}`);
  const largest = parseMegabytes($('max-mb').value);
  if (largest.error) return showMessage(error, largest.error);
  // Only what changed is sent, so a manager of an asset library never sends its kind.
  const change = {};
  const description = $('description').value.trim();
  if (description !== (library.description ?? '')) change.description = description;
  if (!$('kind').disabled && $('kind').value !== library.kind) change.kind = $('kind').value;
  if ($('folders').checked !== library.folders) change.folders = $('folders').checked;
  const allowedTypes = allowed.types.length ? allowed.types : null;
  if (!sameList(allowedTypes, library.allowed_types)) change.allowed_types = allowedTypes;
  if (!sameList(blocked.types, library.blocked_types)) change.blocked_types = blocked.types;
  if (largest.bytes !== (library.max_file_bytes ?? null)) change.max_file_bytes = largest.bytes;
  if (Object.keys(change).length === 0) {
    show(library);
    return showMessage($('saved'), 'Nothing has changed.');
  }
  const result = await api('PATCH', `/libraries/${seg(libraryId)}`, change);
  if (!result.ok) {
    const text =
      result.error === 'invalid_library_settings' ? libraryReasonText(result.data?.reason) : messageFor(result.error);
    return showMessage(error, text);
  }
  show(result.data);
  showMessage($('saved'), 'Saved. Files already in the library stay; any the new settings refuse are marked.');
});

/** Fills the connected repository form without ever putting a token into it. */
function showSource() {
  $('source-section').hidden = source === null;
  if (source === null) return;
  $('source-repository').textContent = `github.com/${source.repository}`;
  $('source-branch').value = source.branch;
  $('source-folder').value = source.folder ?? '';
  $('source-poll').value = String(source.poll_seconds);
  $('source-token').value = '';
  $('source-token-state').textContent = source.token_set ? '— set' : '— not set';
  $('source-token-remove').hidden = !source.token_set;
  $('webhook-state').textContent = source.webhook?.configured
    ? 'A webhook secret is configured. Creating another replaces it.'
    : 'No webhook secret is configured.';
}

$('source-settings').addEventListener('submit', async (event) => {
  event.preventDefault();
  if (source === null) return;
  showMessage($('source-error'), '');
  showMessage($('source-saved'), '');
  const branch = $('source-branch').value.trim();
  const folder = $('source-folder').value.trim();
  const pollSeconds = Number($('source-poll').value);
  if (!branch) {
    showMessage($('source-error'), 'Give the branch a name.');
    $('source-branch').focus();
    return;
  }
  if (!Number.isInteger(pollSeconds) || pollSeconds < 60 || pollSeconds > 86_400) {
    showMessage($('source-error'), 'Check every must be a whole number from 60 to 86,400 seconds.');
    $('source-poll').focus();
    return;
  }
  const change = {};
  if (branch !== source.branch) change.branch = branch;
  if (folder !== (source.folder ?? '')) change.folder = folder;
  if (pollSeconds !== source.poll_seconds) change.poll_seconds = pollSeconds;
  const token = $('source-token').value;
  if (token !== '') change.token = token;
  $('source-token').value = '';
  if (Object.keys(change).length === 0) {
    showSource();
    showMessage($('source-saved'), 'Nothing has changed.');
    return;
  }
  const result = await api('PATCH', `/libraries/${seg(libraryId)}/source`, change);
  if (!result.ok) {
    showMessage($('source-error'), sourceError(result.error));
    return;
  }
  source = result.data;
  showSource();
  showMessage($('source-saved'), 'Source settings saved. An update has started.');
});

$('source-token-replace').addEventListener('click', () => {
  showMessage($('source-error'), '');
  if ($('source-token').value === '') {
    showMessage($('source-error'), 'Enter the replacement token.');
    $('source-token').focus();
    return;
  }
  $('source-settings').requestSubmit();
});

$('source-token-remove').addEventListener('click', async () => {
  if (source === null || !source.token_set) return;
  showMessage($('source-error'), '');
  showMessage($('source-saved'), '');
  $('source-token').value = '';
  const result = await api('PATCH', `/libraries/${seg(libraryId)}/source`, { clear_token: true });
  if (!result.ok) return showMessage($('source-error'), sourceError(result.error));
  source = result.data;
  showSource();
  showMessage($('source-saved'), 'Token removed. An update has started.');
});

$('webhook-create').addEventListener('click', async () => {
  if (source === null) return;
  showMessage($('webhook-error'), '');
  $('webhook-reveal').hidden = true;
  $('webhook-steps').replaceChildren();
  const result = await api('POST', `/libraries/${seg(libraryId)}/source/webhook`);
  if (!result.ok) return showMessage($('webhook-error'), messageFor(result.error));
  const lines = webhookInstructions(result.data, result.data.secret);
  $('webhook-once').textContent = result.data.replaced
    ? 'The earlier secret was replaced. Copy these details now; they are shown only once.'
    : 'Copy these details now; the secret is shown only once.';
  $('webhook-steps').replaceChildren(...lines.map((line) => h('li', {}, line)));
  $('webhook-reveal').hidden = false;
  source = { ...source, webhook: { ...source.webhook, configured: true } };
  $('webhook-state').textContent = 'A webhook secret is configured. Creating another replaces it.';
});

$('source-disconnect').addEventListener('click', async () => {
  if (source === null) return;
  showMessage($('disconnect-error'), '');
  const sure = await confirmDialog(
    `Disconnect github.com/${source.repository}? Updates from GitHub stop, but the files stay in this library.`,
    'Disconnect repository',
  );
  if (!sure) return $('source-disconnect').focus();
  const result = await api('DELETE', `/libraries/${seg(libraryId)}/source`);
  if (!result.ok) return showMessage($('disconnect-error'), messageFor(result.error));
  source = null;
  $('webhook-steps').replaceChildren();
  $('webhook-once').textContent = '';
  $('webhook-reveal').hidden = true;
  showSource();
  showMessage($('saved'), 'Disconnected from GitHub. The files stay in this library.');
});
