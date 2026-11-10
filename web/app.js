// Wires the text box to the WebAssembly client. All merging happens in the client; this script
// moves frames between the socket and the client and reports what the person typed.

import init, { Client } from './pkg/web_client.js';

const NAME_KEY = 'sharecraft.name';
const FIRST_DELAY = 500;
const MAX_DELAY = 8000;

const $ = (id) => document.getElementById(id);
const frame = $('frame');
const area = $('text');
const nameInput = $('name');
const statusLine = $('status');
const peopleList = $('people');

const docId = documentId();
$('docline').textContent = `ShareCraft development build · Document ${docId}`;

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

nameInput.value = loadName();

try {
  await init();
  client = new Client();
} catch (err) {
  console.error(err);
  setStatus('failed', 'The editor could not be loaded. Run "cargo xtask web" and reload.');
  throw err;
}
connect();

// ---- document and name ----

function documentId() {
  const id = location.hash.slice(1).toLowerCase();
  if (/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(id)) return id;
  const bytes = crypto.getRandomValues(new Uint8Array(16));
  bytes[6] = (bytes[6] & 0x0f) | 0x40;
  bytes[8] = (bytes[8] & 0x3f) | 0x80;
  const hex = Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');
  const fresh = `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
  history.replaceState(null, '', `#${fresh}`);
  return fresh;
}

// Another document was chosen by editing the address.
addEventListener('hashchange', () => location.reload());

function loadName() {
  try {
    return localStorage.getItem(NAME_KEY) ?? '';
  } catch {
    return '';
  }
}

nameInput.addEventListener('change', () => {
  try {
    localStorage.setItem(NAME_KEY, nameInput.value.trim());
  } catch {
    // Storage may be unavailable; the name still applies to this tab.
  }
  if (client) reconnectNow();
});

// ---- socket ----

function connect() {
  clearTimeout(retryTimer);
  const scheme = location.protocol === 'https:' ? 'wss' : 'ws';
  const name = encodeURIComponent(nameInput.value.trim());
  const ws = new WebSocket(`${scheme}://${location.host}/ws/docs/${docId}?name=${name}`);
  socket = ws;
  joined = false;
  ws.onopen = () => {
    if (ws !== socket) return;
    ws.send(client.join_message());
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
    socket = null;
    joined = false;
    lost = !refused;
    refused = false;
    retryTimer = setTimeout(connect, lost ? delay : 0);
    if (lost) delay = Math.min(delay * 2, MAX_DELAY);
    render();
  };
  render();
}

function reconnectNow() {
  const old = socket;
  socket = null;
  lost = false;
  old?.close();
  connect();
}

function send(message) {
  if (message && joined && socket?.readyState === WebSocket.OPEN) socket.send(message);
}

function handleFrame(data) {
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
  }
  if (update.send) send(update.send);
  if (update.text_changed) showRemote(update.text, update.edits);
  if (!everJoined) {
    everJoined = true;
    area.readOnly = false;
  }
  if (update.people) showPeople(update.people);
  if (update.save) save = update.save;
  const n = update.dropped;
  const changes = `${n} ${n === 1 ? 'change' : 'changes'}`;
  if (update.reset === 'restarted_lost_text') {
    // The new room's text differs from what the person had: unsaved text is gone.
    notice = 'The server restarted. Changes made since the last save were lost.';
    if (n > 0) notice += ` ${changes} you typed while disconnected could not be kept.`;
  } else if (n > 0) {
    const kept = `${changes} could not be kept.`;
    notice = notice.startsWith('A change was refused') ? `${notice} ${kept}` : kept;
  }
  if (update.refused) {
    notice = `A change was refused (${update.refused}).`;
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
});

area.addEventListener('input', (event) => {
  if (composing || event.isComposing) return;
  syncLocal();
});

// Reports the difference between what the client knows and what the text box shows.
function syncLocal() {
  if (!client || composing || !everJoined) return;
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
      if (person.me) {
        item.classList.add('me');
        const you = document.createElement('small');
        you.textContent = ' (you)';
        name.append(you);
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
  const waiting = `${n} ${n === 1 ? 'change' : 'changes'} waiting`;
  if (!joined && lost && everJoined) {
    setStatus('offline', `Connection lost. ${waiting}`);
  } else if (!joined) {
    setStatus('connecting', everJoined ? `Reconnecting… ${waiting}` : 'Connecting…');
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
