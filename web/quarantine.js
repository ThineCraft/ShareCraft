// The organisation's quarantine, for administrators: content the malware scanner found a threat
// in (or could not check), with its size and the path of each file holding it. Release asks for a
// reason; Delete asks first and removes every file holding the content, skipping the recycle
// bin. Names, paths and threat names are put in as text only.

import {
  QUARANTINE_DELETE_TEXT,
  api,
  confirmDialog,
  messageFor,
  organisationChrome,
  quarantineRow,
  requireMe,
  seg,
  showMessage,
} from './common.js';

const $ = (id) => document.getElementById(id);
const error = $('error');
const status = $('status');
const dialog = $('release-dialog');
let releasing = null; // the entry the release dialog is for

const me = await requireMe();
$('who').textContent = `Signed in as ${me.name}`;
await organisationChrome();

if (!me.is_admin) {
  status.textContent = 'Only administrators can see the quarantine.';
} else {
  await load();
}

async function load() {
  const result = await api('GET', '/admin/quarantine');
  if (!result.ok) return showMessage(error, messageFor(result.error));
  $('entries').replaceChildren(...result.data.map((entry) => quarantineRow(entry, act)));
  $('empty').hidden = result.data.length > 0;
}

function act(action, entry) {
  if (action === 'release') askRelease(entry);
  else remove(entry);
}

async function remove(entry) {
  if (!(await confirmDialog(QUARANTINE_DELETE_TEXT, 'Delete'))) return;
  showMessage(error, '');
  const result = await api('POST', `/admin/quarantine/${seg(entry.sha256)}/delete`);
  if (!result.ok) return showMessage(error, messageFor(result.error));
  const count = result.data?.documents ?? 0;
  status.textContent = `Deleted ${count} ${count === 1 ? 'file' : 'files'} holding that content.`;
  await load();
}

function askRelease(entry) {
  releasing = entry;
  $('release-text').textContent = entry.threat
    ? `Release content the scanner found ${entry.threat} in? Everyone who can see its files can open them again.`
    : 'Release this content? Everyone who can see its files can open them again.';
  $('release-reason').value = '';
  showMessage($('release-error'), '');
  dialog.showModal();
  $('release-reason').focus();
}

// The buttons act on their own click: a dialog's close event may never come in a hidden tab.
$('release-cancel').addEventListener('click', () => {
  releasing = null;
  dialog.close();
});
$('release-form').addEventListener('submit', (event) => event.preventDefault());
$('release-confirm').addEventListener('click', async () => {
  const entry = releasing;
  if (!entry) return;
  const reason = $('release-reason').value.trim();
  if (reason.length < 3) return showMessage($('release-error'), 'Give a reason of at least 3 characters.');
  const result = await api('POST', `/admin/quarantine/${seg(entry.sha256)}/release`, { reason });
  if (!result.ok) return showMessage($('release-error'), messageFor(result.error));
  releasing = null;
  dialog.close();
  status.textContent = 'Released. Its files can be opened again.';
  await load();
});
