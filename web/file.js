// The frame around an editor: the page that opens a file document (a spreadsheet, a word
// document, a PDF, a design) in its editor, which runs in an <iframe> on this origin. The page
// joins the document's file session, hands the editor the stored bytes, and uploads what the
// editor saves while this page holds the editing seat. The server decides who may do what.

import {
  BRIDGES,
  EDITORS,
  SaveQueue,
  classifySave,
  connect as connectEditor,
  editorUrl,
  nextStatus,
  noticeAfter,
  saveFailure,
  seatBanner,
} from './editor-host.js';
import {
  MISSING_DOCUMENT,
  api,
  goToSignIn,
  h,
  messageFor,
  removedMessage,
  requireMe,
  seg,
  sharing,
} from './common.js';

const FIRST_DELAY = 500;
const MAX_DELAY = 8000;
const READY_TIMEOUT = 60_000;
const COPY_TIMEOUT = 10_000; // how long a requested copy waits for the editor's save
const SEAT_GONE =
  'Someone else is editing this file now, so changes you make here are not saved. Download your copy, then reload.';
const NOT_READY = 'The editor is not ready yet.';
const NOT_READY_FOR = 5000; // how long "not ready yet" stays at most, in milliseconds
const MOVED_ON =
  'A newer version was saved while the connection was lost, so changes you make here are not saved. Download your copy, then reload.';
const NEWER = 'A newer version was saved. Reload to see it.';

const $ = (id) => document.getElementById(id);
const frameBox = $('frame');
const statusPill = frameBox.querySelector('.status');
const statusText = $('status');
const saveButton = $('save');
const downloadLink = $('download');
const shareToggle = $('share-toggle');
const sharePanel = $('share-panel');
const banner = $('banner');
const bannerText = $('banner-text');
const bannerActions = $('banner-actions');
const editorArea = $('editor-area');

// Every piece of state is declared here, before any code below runs.
const clientId = crypto.randomUUID(); // this page load's connection
let me = null;
let docId = '';
let doc = null;             // { id, name, editor, role, revision } from the server
let share = null;           // the Sharing panel
let shareOpen = false;
let socket = null;
let joined = false;         // the server answered the join on the current socket
let everJoined = false;
let delay = FIRST_DELAY;
let retryTimer = 0;
let checking = false;       // a still-allowed check is in flight
let removed = false;        // the server removed this page: nothing is shown or kept
let canEdit = false;        // the server says this person may edit the file
let seat = null;            // the seat as the server last described it, without its token
let seatToken = null;       // proves the seat when saving; never written anywhere else
let holder = false;         // this page holds the editing seat
let savingStopped = '';     // why this page stopped saving for good ('' while it may save)
let lostEdit = false;       // it stopped because it held the seat and lost the right to edit
let knownRevision = null;   // the revision the editor's content is based on
const ours = new Set();     // revisions this page stored
let laterRevisions = [];    // revisions announced while an upload was under way, judged after it
let status = nextStatus(undefined, { type: 'opening' });
let editorDirty = false;    // the editor reports unsaved changes
let frame = null;           // the <iframe>
let bridge = null;
let editorReady = false;
let readyTimer = 0;
let initialBytes = null;    // the stored bytes, until the editor has them
let mine = null;            // the editor's last save that this page will not upload
let downloadNext = false;   // the editor's next save goes to the person as their copy
let downloadNextTimer = 0;
const objectUrls = new Set(); // blob: URLs of downloads not yet revoked
let notice = null;          // a banner that stays until what it is about is over
let bannerKey = '';
let roleCheck = 0;          // the latest role request; older answers are dropped
let notReadyTimer = 0;
const queue = new SaveQueue(upload, onQueue);

addEventListener('beforeunload', (event) => {
  if (removed) return;
  // Unsaved changes in the editor, or a copy the server has not stored.
  if (editorDirty || queue.kept || mine) {
    event.preventDefault();
    event.returnValue = '';
  }
});

saveButton.addEventListener('click', () => {
  if (!bridge?.requestSave()) notReady();
});

shareToggle.addEventListener('click', () => {
  shareOpen = !shareOpen;
  sharePanel.hidden = !shareOpen;
  shareToggle.setAttribute('aria-expanded', String(shareOpen));
});

main().catch((err) => showProblem(err?.message || messageFor('unknown')));

async function main() {
  me = await requireMe();
  docId = documentId();
  if (!docId) {
    showProblem(MISSING_DOCUMENT);
    return;
  }
  share = sharing(docId);
  const detail = await api('GET', `/docs/${seg(docId)}`);
  if (!detail.ok) {
    showProblem(detail.status === 404 ? MISSING_DOCUMENT : messageFor(detail.error));
    return;
  }
  if (detail.data.editor === 'text') {
    // A text document opens in the text page.
    location.replace(`/doc.html#${seg(docId)}`);
    return;
  }
  doc = detail.data;
  knownRevision = doc.revision ?? null;
  showDetail();
  setRole(doc.role);
  showDownload();

  const editors = await api('GET', '/editors');
  if (removed) return;
  if (!editors.ok) {
    statusPill.hidden = true;
    setNotice({ text: messageFor(editors.error), actions: ['download'] });
    return;
  }
  const installed = editors.data.some((editor) => editor.id === doc.editor);
  if (!installed || !Object.hasOwn(BRIDGES, doc.editor)) {
    statusPill.hidden = true;
    setNotice({
      text: `The ${editorName()} editor is not installed on this server.`,
      actions: ['download'],
    });
    return;
  }
  render();
  openSocket();
}

// ---- document ----

// The document id in the address, or '' when the address holds anything else.
function documentId() {
  const id = (new URLSearchParams(location.search).get('doc') ?? '').toLowerCase();
  return /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(id) ? id : '';
}

function editorName() {
  return Object.hasOwn(EDITORS, doc.editor) ? EDITORS[doc.editor].name : doc.editor;
}

function showDetail() {
  $('title').textContent = doc.name;
  $('docline').textContent = `${editorName()} · Your role: ${doc.role}`;
  document.title = `${doc.name} — ShareCraft`;
}

// Owners get Share; everyone else does not.
function setRole(role) {
  const owner = !removed && share.setRole(role);
  shareToggle.hidden = !owner;
  if (!owner) {
    shareOpen = false;
    sharePanel.hidden = true;
    shareToggle.setAttribute('aria-expanded', 'false');
  }
}

function contentUrl(revision) {
  return `/api/docs/${seg(docId)}/content?revision=${encodeURIComponent(revision)}`;
}

// The bar's Download link fetches the revision this page knows; there is none before a save.
function showDownload() {
  downloadLink.hidden = removed || !knownRevision;
  if (knownRevision && !removed) downloadLink.href = contentUrl(knownRevision);
  else downloadLink.removeAttribute('href');
}

function showProblem(text) {
  statusPill.hidden = true;
  editorArea.replaceChildren();
  $('problem').textContent = text;
}

// The server removed this page, or the document is gone: nothing of it stays on the screen or
// in memory, and the page does not reconnect.
function stopRemoved(reason) {
  removed = true;
  clearTimeout(retryTimer);
  clearTimeout(readyTimer);
  queue.drop();
  mine = null;
  initialBytes = null;
  downloadNext = false;
  clearTimeout(downloadNextTimer);
  for (const url of objectUrls) URL.revokeObjectURL(url);
  objectUrls.clear();
  editorDirty = false;
  seatToken = null;
  seat = null;
  holder = false;
  notice = null;
  bridge?.close();
  bridge = null;
  frame?.remove();
  frame = null;
  share?.stop();
  setRole('');
  saveButton.hidden = true;
  showDownload();
  $('title').textContent = 'Document';
  $('docline').textContent = '';
  document.title = 'ShareCraft — Document';
  setBanner(null);
  const old = socket;
  socket = null;
  joined = false;
  old?.close();
  showProblem(reason === 'missing' ? MISSING_DOCUMENT : removedMessage(reason));
}

// ---- socket ----

function openSocket() {
  clearTimeout(retryTimer);
  if (removed) return;
  const scheme = location.protocol === 'https:' ? 'wss' : 'ws';
  const ws = new WebSocket(`${scheme}://${location.host}/ws/docs/${seg(docId)}`);
  socket = ws;
  joined = false;
  ws.onopen = () => {
    if (ws === socket) ws.send(JSON.stringify({ type: 'join', client: clientId }));
  };
  ws.onmessage = (event) => {
    if (ws !== socket || removed) return;
    let message;
    try {
      message = JSON.parse(event.data);
    } catch {
      ws.close();
      return;
    }
    handle(message);
  };
  ws.onclose = () => {
    if (ws === socket) closed();
  };
}

// The socket is gone: the editor stays, saving pauses, and the page joins again later.
function closed() {
  socket = null;
  joined = false;
  if (removed) return;
  queue.pause();
  updateStatus({ type: 'offline' });
  retryTimer = setTimeout(openSocket, delay);
  delay = Math.min(delay * 2, MAX_DELAY);
  checkStillAllowed();
}

// A closed socket may mean the document or the access to it is gone.
async function checkStillAllowed() {
  if (checking) return;
  checking = true;
  const result = await api('GET', `/docs/${seg(docId)}`); // 401 goes to the sign-in page
  checking = false;
  if (!removed && !result.ok && result.status === 404) stopRemoved('missing');
}

function handle(message) {
  switch (message?.type) {
    case 'joined':
      onJoined(message);
      break;
    case 'seat':
      onSeat(message.seat);
      break;
    case 'save':
      if (message.state === 'saved') onSaved(message.revision ?? null);
      break;
    case 'access':
      onAccess(message.can_edit === true);
      break;
    case 'removed':
      stopRemoved(message.reason);
      break;
    default:
      break; // `people`: this slim bar does not list them.
  }
}

function onJoined(message) {
  const first = !everJoined;
  joined = true;
  everJoined = true;
  delay = FIRST_DELAY;
  canEdit = message.can_edit === true;
  const revision = message.revision ?? null;
  updateStatus({ type: 'online' });
  if (first) {
    takeSeat(message.seat);
    knownRevision = revision;
    showDownload();
    render();
    loadEditor();
    return;
  }
  if (savingStopped) {
    seat = withoutToken(message.seat);
    render();
    return;
  }
  const wasHolder = holder;
  if (wasHolder && message.seat?.state !== 'yours') {
    loseSeat(message.seat);
    return;
  }
  takeSeat(message.seat);
  if (holder) {
    judgeRevision(revision, () => stopSaving(wasHolder ? MOVED_ON : NEWER));
  } else {
    judgeRevision(revision, () => setNotice({ kind: 'newer', text: NEWER, actions: ['reload'] }));
  }
  if (!savingStopped) {
    queue.resume();
    // A save refused while the connection was down (its seat token had lapsed, or the server
    // could not be reached) goes again now that the seat is held again.
    const failed = queue.failure?.status;
    if (holder && (failed === 0 || failed === 403 || failed === 503)) queue.retry();
  }
  render();
}

function onSeat(next) {
  if (savingStopped) {
    // A page that lost the right to edit says who has the seat once it may edit again.
    seat = withoutToken(next);
    render();
    return;
  }
  if (holder && next?.state !== 'yours') {
    loseSeat(next);
    return;
  }
  takeSeat(next);
  render();
}

// This page held the seat and no longer does: someone else took it, or the right to edit was
// taken away. Either way it never saves again without a reload. A lost right is told by the
// seat banner, which follows the right to edit if it comes back.
function loseSeat(next) {
  seat = withoutToken(next);
  if (canEdit) {
    stopSaving(SEAT_GONE);
    return;
  }
  savingStopped = 'lost the right to edit';
  lostEdit = true;
  holder = false;
  seatToken = null;
  queue.stop();
  render();
}

// Whether leaving the page now would lose work: unsaved changes in the editor, or a copy the
// server has not stored.
const somethingToLose = () => editorDirty || queue.kept !== null || mine !== null;

// This person's rights changed while connected. The page stays as it is: the role line and the
// Share button follow the server's answer, and the seat follows the server's `seat` message (a
// holder who may no longer edit is sent `seat: free` next, which stops saving).
function onAccess(can) {
  canEdit = can;
  refreshRole();
  render();
}

async function refreshRole() {
  const check = ++roleCheck;
  const result = await api('GET', `/docs/${seg(docId)}`); // 401 goes to the sign-in page
  if (check !== roleCheck || removed) return;
  if (result.ok) {
    doc = { ...doc, name: result.data.name, role: result.data.role };
    showDetail();
    setRole(doc.role);
  } else if (result.status === 404) {
    stopRemoved('missing');
  }
}

function onSaved(revision) {
  judgeRevision(revision, () => {
    if (holder && !savingStopped) stopSaving(NEWER);
    else setNotice({ kind: 'newer', text: NEWER, actions: ['reload'] });
  });
}

// Calls `newer` if `revision` is one this page does not have. While an upload is under way the
// revision may be the one it is storing, so the question waits for its answer.
function judgeRevision(revision, newer) {
  if (revision == null || revision === knownRevision || ours.has(revision)) return;
  if (queue.busy) {
    laterRevisions.push([revision, newer]);
    return;
  }
  newer();
}

function withoutToken(next) {
  if (next?.state === 'taken') {
    return { state: 'taken', by: { name: String(next.by?.name ?? ''), user: String(next.by?.user ?? '') } };
  }
  return { state: next?.state === 'yours' ? 'yours' : next?.state === 'free' ? 'free' : 'unknown' };
}

function takeSeat(next) {
  seat = withoutToken(next);
  holder = next?.state === 'yours' && typeof next.token === 'string' && !savingStopped;
  seatToken = holder ? next.token : null;
}

const canSave = () => holder && !savingStopped && !removed;
// Whether this page is the one editing, for the status: a page that stopped saving says
// "Not saved", not "View only", unless it lost the right to edit with nothing to lose.
const editing = () => (holder || (savingStopped !== '' && (!lostEdit || somethingToLose()))) && !removed;

// The page will not upload again: it says why and offers the person's copy and a reload.
function stopSaving(text) {
  savingStopped = text;
  holder = false;
  seatToken = null;
  queue.stop();
  updateStatus({ type: 'not_stored' });
  setNotice({ kind: 'stopped', text, actions: ['mycopy', 'reload'] });
}

// ---- editor ----

async function loadEditor() {
  let bytes = null;
  if (knownRevision) {
    const result = await fetchContent(knownRevision);
    if (removed) return;
    if (!result.ok) {
      if (result.status === 404) checkStillAllowed();
      setNotice({ text: `The file could not be downloaded. ${messageFor(result.error)}`, actions: ['reload'] });
      return;
    }
    bytes = result.bytes;
  }
  initialBytes = bytes;
  startEditor();
}

async function fetchContent(revision) {
  let response;
  try {
    response = await fetch(contentUrl(revision), { credentials: 'same-origin' });
  } catch {
    return { ok: false, status: 0, error: 'network' };
  }
  if (response.status === 204) return { ok: true, bytes: null };
  if (!response.ok) {
    const data = await response.json().catch(() => null);
    const error = typeof data?.error === 'string' ? data.error : 'unknown';
    if (response.status === 401 && error === 'not_signed_in') goToSignIn();
    return { ok: false, status: response.status, error };
  }
  try {
    return { ok: true, bytes: new Uint8Array(await response.arrayBuffer()) };
  } catch {
    return { ok: false, status: 0, error: 'network' };
  }
}

function startEditor() {
  const element = h('iframe', {
    title: `${editorName()} editor`,
    allow: 'clipboard-read; clipboard-write',
  });
  // The bridge listens before the editor can say it is ready.
  bridge = connectEditor(element, BRIDGES[doc.editor], {
    ready: onReady,
    save: onEditorSave,
    dirty: onDirty,
    failed: ({ error, notAnswering }) => {
      if (removed) return;
      // Not answering in time passes: a later save that goes through settles it. A stop or a
      // save failure stays on the screen with its actions.
      if (notAnswering) setNotice(noticeAfter(notice, { kind: 'not_answering', text: error }));
      else editorFailed(error ? `The editor reported a problem: ${error}` : 'The editor reported a problem.');
    },
  });
  readyTimer = setTimeout(() => editorFailed('The editor did not start within 60 seconds.'), READY_TIMEOUT);
  element.src = editorUrl(doc.editor, me.name);
  frame = element;
  editorArea.replaceChildren(element);
}

function onReady() {
  if (editorReady || removed) return;
  editorReady = true;
  clearTimeout(readyTimer);
  clearNotReady();
  // A document that was never saved has no bytes: the editor starts empty (one that opens on
  // a start screen is asked for a blank document).
  bridge.start(doc.name, initialBytes);
  initialBytes = null;
  updateStatus({ type: 'opened', canSave: editing() });
  render();
}

function onDirty({ dirty }) {
  if (removed) return;
  editorDirty = dirty;
  updateStatus({ type: 'dirty', dirty });
  // The editor is answering; and whether there is a copy to offer may have changed.
  if (!clearNotReady()) render();
}

function editorFailed(text) {
  clearTimeout(readyTimer);
  if (removed) return;
  setNotice({ kind: 'editor', text, actions: ['download'] });
}

function onEditorSave({ name, bytes }) {
  if (removed) return;
  if (downloadNext) {
    downloadNext = false;
    clearTimeout(downloadNextTimer);
    editorDirty = false;
    mine = bytes;
    downloadBytes(bytes, doc.name);
    return;
  }
  if (classifySave(EDITORS[doc.editor]?.extension, name) === 'export') {
    // An export leaves the document as it was: its own `dirty` messages still count.
    downloadBytes(bytes, name || 'export');
    return;
  }
  // The editor counts its document saved once it hands the bytes over.
  editorDirty = false;
  if (savingStopped) {
    // The stopped queue keeps it as the newest copy and uploads nothing.
    queue.offer(bytes);
    repeatBanner();
    return;
  }
  if (!canSave()) {
    mine = bytes;
    repeatBanner();
    return;
  }
  queue.offer(bytes);
}

// ---- saving ----

// Uploads `bytes` with the seat token, based on the revision this page last knew.
async function upload(bytes) {
  const token = seatToken;
  if (!token || removed) return { ok: false, status: 403 };
  const base = knownRevision ? `?base=${encodeURIComponent(knownRevision)}` : '';
  let response;
  try {
    response = await fetch(`/api/docs/${seg(docId)}/content${base}`, {
      method: 'PUT',
      credentials: 'same-origin',
      headers: { 'X-ShareCraft-Seat': token, 'Content-Type': 'application/octet-stream' },
      body: bytes,
    });
  } catch {
    return { ok: false, status: 0 };
  }
  const data = await response.json().catch(() => null);
  const error = typeof data?.error === 'string' ? data.error : null;
  if (response.status === 401 && error === 'not_signed_in') goToSignIn();
  if (response.ok && typeof data?.revision === 'string') {
    if (!removed) {
      ours.add(data.revision);
      knownRevision = data.revision;
      showDownload();
    }
    return { ok: true };
  }
  // The seat was given to this page again (a rejoin) while the upload was under way: the old
  // token was refused, the new one may not be.
  const staleSeat = response.status === 403 && seatToken !== null && seatToken !== token;
  return { ok: false, status: response.ok ? 0 : response.status, error, staleSeat };
}

function onQueue(event) {
  if (removed) return;
  if (event.type === 'saving') {
    if (!savingStopped) updateStatus({ type: 'saving' });
    if (notice?.kind === 'save' || notice?.kind === 'not_ready') setNotice(null);
  } else if (event.type === 'stored') {
    if (!savingStopped) updateStatus({ type: 'stored' });
    // The editor hears that its save is stored (an editor that needs telling); a save that
    // went through also settles an earlier save problem or an editor that did not answer.
    bridge?.stored(event.bytes);
    if (notice?.kind === 'save' || notice?.kind === 'not_answering') setNotice(null);
  } else if (event.type === 'not_stored') {
    if (event.result.staleSeat && canSave()) {
      // Sent again with the new token: the refused save, or the newer one already waiting.
      if (queue.failure) queue.retry();
    } else if (!savingStopped) {
      const failure = saveFailure(event.result, queue.failure !== null);
      if (failure.stop) {
        // The file moved on: this page load does not save again.
        stopSaving(failure.text);
      } else {
        updateStatus({ type: 'not_stored' });
        setNotice({ kind: 'save', text: failure.text, actions: failure.actions });
      }
    }
  }
  // Revisions announced during the upload can be judged now.
  const later = laterRevisions;
  laterRevisions = [];
  for (const [revision, newer] of later) judgeRevision(revision, newer);
}

// The bytes the editor saved and the server does not have, downloaded under the document's
// name. With none kept, the editor is asked to save, and that save is the copy.
function downloadMyCopy() {
  const bytes = queue.kept ?? mine;
  if (bytes) {
    downloadBytes(bytes, doc.name);
    return;
  }
  downloadNext = Boolean(bridge?.requestSave());
  clearTimeout(downloadNextTimer);
  // An editor that posts no save does not leave a later real save diverted to a download.
  if (downloadNext) downloadNextTimer = setTimeout(() => (downloadNext = false), COPY_TIMEOUT);
  else notReady();
}

// The editor could not be asked to save: its interface is not there yet.
// It goes once the editor next reports in, a save starts, or after a few seconds, so that it
// never stays over the banner about the seat.
function notReady() {
  setNotice(noticeAfter(notice, { kind: 'not_ready', text: NOT_READY }));
  clearTimeout(notReadyTimer);
  notReadyTimer = setTimeout(clearNotReady, NOT_READY_FOR);
}

// Takes a "not ready yet" notice away; returns whether there was one.
function clearNotReady() {
  clearTimeout(notReadyTimer);
  if (notice?.kind !== 'not_ready') return false;
  setNotice(null);
  return true;
}

function downloadBytes(bytes, name) {
  const url = URL.createObjectURL(new Blob([bytes], { type: 'application/octet-stream' }));
  const link = h('a', { href: url, download: name, hidden: true });
  document.body.append(link);
  link.click();
  link.remove();
  objectUrls.add(url);
  setTimeout(() => {
    URL.revokeObjectURL(url);
    objectUrls.delete(url);
  }, 60_000);
}

// ---- status and banner ----

function updateStatus(event) {
  status = nextStatus(status, event);
  frameBox.dataset.state = status.tone;
  if (statusText.textContent !== status.text) statusText.textContent = status.text;
}

function setNotice(next) {
  notice = next;
  render();
}

function render() {
  if (removed) return;
  saveButton.hidden = !(canSave() && editorReady);
  // A page that lost the right to edit with something still to lose says "Not saved".
  if (lostEdit && somethingToLose() && !status.failed) updateStatus({ type: 'not_stored' });
  if (editorReady) updateStatus({ type: 'seat', canSave: editing() });
  // When nothing else needs saying: why this page does not save, with the person's copy when
  // there is one to lose.
  const quiet = seatBanner({
    joined: everJoined,
    holder,
    stopped: savingStopped !== '',
    lostEdit,
    canEdit,
    seat,
    me: typeof me?.id === 'string' ? me.id : '',
    mine: mine !== null || queue.kept !== null,
    unsaved: editorDirty,
  });
  setBanner(notice ?? quiet);
}

// Says the banner again, so that it is announced again.
function repeatBanner() {
  bannerKey = '';
  bannerText.textContent = '';
  setTimeout(render, 150);
}

function setBanner(next) {
  const actions = (next?.actions ?? []).filter((kind) => kind !== 'download' || knownRevision);
  const key = next ? `${next.text}\n${actions.join(',')}` : '';
  if (key === bannerKey) return;
  bannerKey = key;
  bannerText.textContent = next?.text ?? '';
  bannerActions.replaceChildren(...actions.map(actionControl));
  banner.classList.toggle('idle', !next);
}

function actionControl(kind) {
  if (kind === 'download') {
    return h('a', { class: 'button plain', href: contentUrl(knownRevision), download: '' }, 'Download');
  }
  const [label, run] = {
    retry: ['Try again', () => queue.retry()],
    mycopy: ['Download my copy', downloadMyCopy],
    reload: ['Reload', () => location.reload()],
  }[kind];
  const button = h('button', { type: 'button', class: 'plain' }, label);
  button.addEventListener('click', run);
  return button;
}
