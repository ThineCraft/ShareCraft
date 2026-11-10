// Wires the text box to the WebAssembly client. All merging happens in the client; this script
// moves frames between the socket and the client and reports what the person typed.

import init, { Client } from './pkg/web_client.js';
import { api, h, messageFor, requireMe, seg, showMessage } from './common.js';

const FIRST_DELAY = 500;
const MAX_DELAY = 8000;
const MISSING = 'This document does not exist, or you do not have access to it.';
const REMOVED = Object.assign(Object.create(null), {
  access_revoked: 'Your access to this document was removed.',
  signed_out: 'You were signed out.',
  account_disabled: 'Your account was disabled.',
});
const CONFLICT = 'This document was changed somewhere else. Copy anything you need, then reload.';

const $ = (id) => document.getElementById(id);
const frame = $('frame');
const area = $('text');
const statusLine = $('status');
const peopleList = $('people');
const readonlyNote = $('readonly-note');
const sharePanel = $('share-panel');

let client = null;
let socket = null;
let joined = false;     // the server answered the join on the current socket
let everJoined = false; // the text box holds a real document
let lost = false;       // the last socket closed and a reconnect is scheduled
let delay = FIRST_DELAY;
let retryTimer = 0;
let shown = '';         // the text box value as the client last knew it
let composing = false;
let queued = [];        // frames that arrived during composition, with their socket
let save = null;        // { state, revision } from the client
let notice = '';        // a message that stays until the person edits again
let refused = false;    // a change was refused and the socket is being renewed
let pendingClose = null; // a socket that closed during a composition, handled when it ends
let canEdit = false;   // the server says this person may change the text
let removedReason = ''; // set once the server removed this page: no reconnecting

// Another document was chosen by editing the address.
addEventListener('hashchange', () => location.reload());

await requireMe();
const docId = documentId();
if (!docId) {
  showProblem(MISSING);
  throw new Error('no document');
}
const detail = await api('GET', `/docs/${seg(docId)}`);
if (!detail.ok) {
  showProblem(detail.status === 404 ? MISSING : messageFor(detail.error));
  throw new Error(`cannot open the document (${detail.error})`);
}
showDetail(detail.data);
$('editor').hidden = false;
setRole(detail.data.role);

try {
  await init();
  client = new Client();
} catch (err) {
  console.error(err);
  setStatus('failed', 'The editor could not be loaded. Run "cargo xtask web" and reload.');
  throw err;
}
connect();

// ---- document and sharing ----

// The document id in the address, or '' when the address holds anything else.
function documentId() {
  const id = location.hash.slice(1).toLowerCase();
  return /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(id) ? id : '';
}

function showProblem(text) {
  $('editor').hidden = true;
  sharePanel.hidden = true;
  showMessage($('problem'), text);
  setStatus('failed', 'Not available');
}

function showDetail(info) {
  $('title').textContent = info.name;
  $('docline').textContent = `Your role: ${info.role}`;
  document.title = `${info.name} — ShareCraft`;
}

// Owners get the Share panel; everyone else does not.
function setRole(role) {
  const owner = role === 'owner' && removedReason === '';
  sharePanel.hidden = !owner;
  if (owner) showAccess();
}

// Asks the server what this person's role is now (their rights changed) and shows or hides
// the Share panel to match.
let roleCheck = 0;
async function refreshRole() {
  const check = ++roleCheck;
  const result = await api('GET', `/docs/${seg(docId)}`);
  if (check !== roleCheck || removedReason) return;
  if (result.ok) {
    showDetail(result.data);
    setRole(result.data.role);
  } else {
    sharePanel.hidden = true;
  }
}

let accessCheck = 0;
async function showAccess() {
  const check = ++accessCheck;
  const result = await api('GET', `/docs/${seg(docId)}/access`);
  if (check !== accessCheck || removedReason) return;
  if (!result.ok) {
    shareMessage(messageFor(result.error), true);
    return;
  }
  $('access').replaceChildren(
    ...result.data.map(({ user, role }) => {
      const select = h('select', { 'aria-label': `Role of ${user.name}` });
      for (const value of ['viewer', 'editor', 'owner']) {
        const option = h('option', { value }, value[0].toUpperCase() + value.slice(1));
        option.selected = value === role;
        select.append(option);
      }
      select.addEventListener('change', () => changeAccess(user.email, select.value));
      const remove = h(
        'button',
        { type: 'button', class: 'plain', 'aria-label': `Remove ${user.name}` },
        'Remove',
      );
      remove.addEventListener('click', () => removeAccess(user));
      return h(
        'li',
        {},
        h('span', { class: 'grow' }, user.name, h('br'), h('small', {}, user.email)),
        select,
        remove,
      );
    }),
  );
}

async function changeAccess(email, role) {
  shareMessage('', false);
  const result = await api('PUT', `/docs/${seg(docId)}/access`, { email, role });
  if (!result.ok) shareMessage(messageFor(result.error), true);
  else shareMessage('Saved.', false);
  await showAccess();
}

async function removeAccess(user) {
  shareMessage('', false);
  const result = await api('DELETE', `/docs/${seg(docId)}/access/${seg(user.id)}`);
  if (!result.ok) shareMessage(messageFor(result.error), true);
  else shareMessage(`${user.name} was removed.`, false);
  await showAccess();
}

$('share-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  const email = $('share-email').value.trim();
  const result = await api('PUT', `/docs/${seg(docId)}/access`, {
    email,
    role: $('share-role').value,
  });
  if (result.ok) {
    $('share-email').value = '';
    shareMessage('Shared.', false);
  } else {
    shareMessage(messageFor(result.error), true);
  }
  await showAccess();
});

function shareMessage(text, isError) {
  const line = $('share-status');
  line.classList.toggle('ok', !isError);
  showMessage(line, text);
}

// What the text box allows, from what the server last said. In conflict nothing typed can be
// kept, so nothing can be typed.
function applyEditable() {
  area.readOnly =
    !(everJoined && canEdit) || removedReason !== '' || save?.state === 'conflict';
  readonlyNote.hidden = canEdit || !everJoined || removedReason !== '';
}

// The server removed this page: it shows nothing, takes nothing and does not reconnect.
function stopRemoved(reason) {
  removedReason = reason;
  clearTimeout(retryTimer);
  queued = [];
  canEdit = false;
  area.value = '';
  shown = '';
  applyEditable();
  peopleList.replaceChildren();
  sharePanel.hidden = true;
  accessCheck++;
  roleCheck++;
  $('access').replaceChildren();
  shareMessage('', false);
  $('title').textContent = 'Document';
  $('docline').textContent = '';
  document.title = 'ShareCraft — Document';
  const old = socket;
  socket = null;
  joined = false;
  old?.close();
  render();
}

// ---- socket ----

function connect() {
  clearTimeout(retryTimer);
  if (removedReason) return;
  const scheme = location.protocol === 'https:' ? 'wss' : 'ws';
  const ws = new WebSocket(`${scheme}://${location.host}/ws/docs/${docId}`);
  socket = ws;
  joined = false;
  ws.onopen = () => {
    if (ws !== socket) return;
    const join = client.join_message();
    if (join == null) return; // the server removed this page: it must not join again
    ws.send(join);
  };
  ws.onmessage = (event) => {
    if (ws !== socket) return;
    if (composing) {
      queued.push([ws, event.data]);
      return;
    }
    handleFrame(event.data);
  };
  ws.onclose = () => {
    if (ws !== socket) return;
    // Frames held back for a composition may say the server removed this page; they are
    // handled when the composition ends, and only then does the close take effect.
    if (composing && queued.some(([w]) => w === ws)) {
      pendingClose = ws;
      return;
    }
    closed(ws);
  };
  render();
}

// The socket `ws` is gone: reconnect, unless the page was removed.
function closed(ws) {
  if (ws !== socket) return;
  const hadJoined = joined;
  socket = null;
  joined = false;
  if (removedReason) return;
  lost = !refused;
  refused = false;
  retryTimer = setTimeout(connect, lost ? delay : 0);
  if (lost) delay = Math.min(delay * 2, MAX_DELAY);
  render();
  if (!hadJoined && lost) checkStillAllowed();
}

// A reconnect that fails before joining may mean the access or session is gone.
let checking = false;
let closedBeforeJoin = 0; // closes in a row, before joining, while the document stayed open
async function checkStillAllowed() {
  if (checking) return;
  checking = true;
  const result = await api('GET', `/docs/${seg(docId)}`); // 401 goes to the sign-in page
  checking = false;
  if (removedReason) return;
  if (result.ok && ++closedBeforeJoin >= 2) startOver();
  if (result.ok || result.status !== 404) return;
  stopRemoved('missing');
  showProblem(MISSING);
}

// The server keeps closing the connection before the join although this person may open the
// document: the room refuses this page's client id, which another account used there first.
// Start again with a fresh client, as after a reset: the text comes again from the server, and
// typing it had not accepted is not kept.
function startOver() {
  closedBeforeJoin = 0;
  const n = client.pending();
  client.free();
  client = new Client();
  everJoined = false;
  area.value = '';
  shown = '';
  save = null;
  applyEditable();
  if (n > 0) notice = `${n} ${n === 1 ? 'change' : 'changes'} could not be kept.`;
  render();
}

function send(message) {
  if (message && joined && socket?.readyState === WebSocket.OPEN) socket.send(message);
}

function handleFrame(data) {
  if (removedReason) return;
  syncLocal();
  let update;
  try {
    update = JSON.parse(client.receive(data));
  } catch (err) {
    // Nothing changed in the client; joining again resumes or resets it.
    console.error(err);
    socket?.close();
    return;
  }
  if (!joined) {
    joined = true;
    lost = false;
    delay = FIRST_DELAY;
    closedBeforeJoin = 0;
  }
  if (update.removed != null) {
    stopRemoved(update.removed);
    return;
  }
  if (update.send) send(update.send);
  if (update.text_changed) showRemote(update.text, update.edits);
  const firstJoin = !everJoined;
  everJoined = true;
  if (update.can_edit != null) canEdit = update.can_edit;
  if (update.save) save = update.save;
  applyEditable();
  // Their rights changed while the page was open: the Share panel may have to appear or go.
  if (update.can_edit != null && !firstJoin) refreshRole();
  if (update.people) showPeople(update.people);
  const n = update.dropped;
  const changes = `${n} ${n === 1 ? 'change' : 'changes'}`;
  if (update.reset === 'reopened_lost_text') {
    // The document was opened again from its last save, which is older than the text the
    // server had confirmed: accepted changes are gone.
    notice = 'This document was reopened from its last save. Changes made after that save were lost.';
    if (n > 0) notice += ` ${changes} you typed while disconnected could not be kept.`;
  } else if (n > 0) {
    const kept = `${changes} could not be kept.`;
    notice = notice.startsWith('A change was refused') ? `${notice} ${kept}` : kept;
  }
  if (update.refused) {
    notice =
      update.refused === 'read_only'
        ? 'A change was refused because you can no longer edit this document.'
        : `A change was refused (${update.refused}).`;
    refused = true;
    socket?.close();
  }
  render();
}

// ---- text box ----

area.addEventListener('compositionstart', () => {
  composing = true;
});

area.addEventListener('compositionend', () => {
  composing = false;
  syncLocal();
  const frames = queued;
  queued = [];
  for (const [ws, data] of frames) {
    if (ws === socket) handleFrame(data);
  }
  if (pendingClose) {
    const ws = pendingClose;
    pendingClose = null;
    closed(ws);
  }
});

area.addEventListener('input', (event) => {
  if (composing || event.isComposing) return;
  syncLocal();
});

// Reports the difference between what the client knows and what the text box shows.
function syncLocal() {
  if (!client || composing || !everJoined || removedReason) return;
  const value = area.value;
  if (value === shown) return;
  const { start, removed, inserted } = difference(shown, value);
  let message;
  let tooLarge = false;
  try {
    message = client.local_replace(start, removed, inserted);
  } catch (err) {
    // The client refused the change and is unchanged; the text box goes back to its text.
    tooLarge = String(err?.message ?? err).startsWith('change_too_large');
    if (!tooLarge) console.error(err);
    message = undefined;
  }
  shown = client.text();
  if (shown !== value) {
    area.value = shown;
    // The caret goes where the refused change began.
    if (tooLarge) area.setSelectionRange(start, start);
  }
  notice = tooLarge ? 'That change is too large to send in one piece. Paste it in smaller parts.' : '';
  send(message);
  render();
}

// The one replacement turning `before` into `after`, without splitting a surrogate pair.
function difference(before, after) {
  const max = Math.min(before.length, after.length);
  let prefix = 0;
  while (prefix < max && before.charCodeAt(prefix) === after.charCodeAt(prefix)) prefix++;
  if (prefix > 0 && isHigh(before.charCodeAt(prefix - 1))) prefix--;
  let suffix = 0;
  while (
    suffix < max - prefix &&
    before.charCodeAt(before.length - 1 - suffix) === after.charCodeAt(after.length - 1 - suffix)
  ) suffix++;
  if (suffix > 0 && isLow(before.charCodeAt(before.length - suffix))) suffix--;
  return {
    start: prefix,
    removed: before.length - prefix - suffix,
    inserted: after.slice(prefix, after.length - suffix),
  };
}

const isHigh = (unit) => unit >= 0xd800 && unit <= 0xdbff;
const isLow = (unit) => unit >= 0xdc00 && unit <= 0xdfff;

// Shows text changed by someone else, keeping the selection on the same characters.
function showRemote(text, edits) {
  let start = area.selectionStart;
  let end = area.selectionEnd;
  const direction = area.selectionDirection;
  for (const edit of edits) {
    start = moveThrough(start, edit);
    end = moveThrough(end, edit);
  }
  const scroll = area.scrollTop;
  area.value = text;
  shown = text;
  area.setSelectionRange(start, end, direction);
  area.scrollTop = scroll;
}

function moveThrough(pos, { start_utf16: at, removed_utf16: removed, inserted_utf16: inserted }) {
  if (removed > 0) {
    if (pos >= at + removed) pos -= removed;
    else if (pos > at) pos = at;
  }
  if (inserted > 0 && at <= pos) pos += inserted;
  return pos;
}

// ---- people and status ----

function showPeople(people) {
  peopleList.replaceChildren(
    ...people.map((person) => {
      const item = document.createElement('li');
      const badge = document.createElement('span');
      badge.className = 'av';
      badge.setAttribute('aria-hidden', 'true');
      badge.textContent = initials(person.name);
      const name = document.createElement('span');
      name.textContent = person.name;
      item.append(badge, name);
      const tags = [person.me ? 'you' : '', person.can_edit ? '' : 'read only'].filter(Boolean);
      if (person.me) item.classList.add('me');
      if (tags.length > 0) {
        const small = document.createElement('small');
        small.textContent = ` (${tags.join(', ')})`;
        name.append(small);
      }
      return item;
    }),
  );
  if (people.length === 0) {
    const item = document.createElement('li');
    item.className = 'empty';
    item.textContent = 'Nobody is connected.';
    peopleList.append(item);
  }
}

function initials(name) {
  const words = name.trim().split(/\s+/u).filter(Boolean);
  const letters = words.slice(0, 2).map((w) => Array.from(w)[0] ?? '');
  return letters.join('').toUpperCase() || '?';
}

function render() {
  if (!client) return;
  const n = client.pending();
  if (removedReason) {
    setStatus('failed', REMOVED[removedReason] ?? 'Your access to this document ended.');
    return;
  }
  const waiting = `${n} ${n === 1 ? 'change' : 'changes'} waiting`;
  if (!joined && lost && everJoined) {
    setStatus('offline', `Connection lost. ${waiting}`);
  } else if (!joined) {
    setStatus('connecting', everJoined ? `Reconnecting… ${waiting}` : 'Connecting…');
  } else if (save?.state === 'conflict') {
    setStatus('failed', CONFLICT);
  } else if (save?.state === 'failed') {
    // A failing save is never hidden by a notice; both are shown.
    setStatus('failed', notice ? `Saving failed, retrying. ${notice}` : 'Saving failed, retrying');
  } else if (notice) {
    setStatus('notice', notice);
  } else if (n > 0 || save?.state === 'saving') {
    setStatus('saving', 'Saving…');
  } else {
    setStatus('saved', 'Saved');
  }
}

function setStatus(state, text) {
  frame.dataset.state = state;
  if (statusLine.textContent !== text) statusLine.textContent = text;
}
