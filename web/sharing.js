// The organisation's sharing settings, for administrators: whether links for anyone may be made
// and used, and how long one may last. The server decides; refusals are shown as sentences.

import { api, messageFor, requireMe, showMessage } from './common.js';

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

if (!me.is_admin) {
  status.textContent = 'Only administrators can change the sharing settings.';
} else {
  const result = await api('GET', '/settings/sharing');
  if (result.ok) {
    show(result.data);
    $('settings').hidden = false;
  } else {
    showMessage(error, messageFor(result.error));
  }
}

function show(settings) {
  $('anyone-links').checked = settings.anyone_links;
  $('max-days').value = String(settings.anyone_link_max_days);
}

function note(text, isError) {
  error.classList.toggle('ok', !isError);
  showMessage(error, text);
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
