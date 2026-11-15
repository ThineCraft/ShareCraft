// The landing page of a sharing link. It works without an account and without the other pages'
// session code: an anyone link shows the shared item (a file to download or read, or a folder to
// browse) and nothing else; a signed-in link asks the visitor to confirm before joining it.
// The token is in the landing page's own address and nowhere else. A signed-out visitor sent to
// sign in has it kept in sessionStorage for the trip (sign-in returns to /link.html, which holds
// no token); it is removed as soon as it is read.

import {
  LINK_NOT_WORKING,
  fileOffer,
  h,
  itemMarks,
  messageFor,
  previewKindOf,
  safeNext,
  seg,
  showMessage,
} from './common.js';

const $ = (id) => document.getElementById(id);
const PAGE = 50;

const STASH = 'sharecraft-link';
const TOKEN = /^[0-9a-f]{64}$/;

/** The token in this page's address, or else the one kept for the sign-in trip (and forgotten). */
function readToken() {
  const own = /^\/l\/([0-9a-f]{64})\/?$/.exec(location.pathname)?.[1];
  let kept = '';
  try {
    kept = sessionStorage.getItem(STASH) ?? '';
    sessionStorage.removeItem(STASH);
  } catch {
    kept = '';
  }
  return own ?? (TOKEN.test(kept) ? kept : '');
}

const token = readToken();
const base = `/api/l/${token}`;

/** One call to the link routes. Never throws; unlike the pages' `api`, a 401 is not sent to sign in. */
async function call(method, path, credentials) {
  let response;
  try {
    response = await fetch(`${base}${path}`, { method, credentials });
  } catch {
    return { ok: false, status: 0, data: null, error: 'network' };
  }
  let data = null;
  if (response.status !== 204) {
    try {
      data = await response.json();
    } catch {
      data = null;
    }
  }
  const error = response.ok ? null : typeof data?.error === 'string' ? data.error : 'unknown';
  return { ok: response.ok, status: response.status, data, error };
}

/** Shows only the section called `name`. */
function only(name) {
  for (const id of ['broken', 'signin', 'open', 'item']) $(id).hidden = id !== name;
  $('status').textContent = '';
}

/** The page for any link that does not work, whatever the reason. */
function notWorking() {
  only('broken');
  showMessage($('problem'), LINK_NOT_WORKING);
}

// The anyone-link page's state. Declared before the start-up code below: that code awaits, then
// calls showAnyone, which uses all of it (a `let` or `const` further down would still be unusable).
let trail = []; // the folders opened, from the shared item down: { id, name }
let items = [];
let next = null;
let loading = 0; // the latest folder load; answers to older ones are dropped

const contentUrl = (id) => `${base}/content/${seg(id)}`;
const previewUrl = (id) => `${base}/preview/${seg(id)}`;
let previewing = 0; // the latest preview asked for; older answers are dropped

if (!token) {
  notWorking();
} else {
  const found = await call('GET', '', 'same-origin');
  if (found.ok && found.data.kind === 'anyone') showAnyone(found.data);
  else if (found.ok && found.data.kind === 'signed_in') showSignedIn(found.data);
  else if (found.status === 401 && found.error === 'not_signed_in') showSignInFirst();
  else if (found.error === 'network' || found.status >= 500) {
    only('broken');
    showMessage($('problem'), messageFor(found.error === 'network' ? 'network' : 'unavailable'));
  } else notWorking();
}

// ---- a signed-in link ----

function showSignInFirst() {
  only('signin');
  // The person comes back to /link.html after signing in; the token travels in sessionStorage,
  // never in another address.
  let kept = true;
  try {
    sessionStorage.setItem(STASH, token);
  } catch {
    kept = false;
  }
  $('signin-link').href = kept ? `/signin.html?next=${encodeURIComponent('/link.html')}` : '/signin.html';
}

function showSignedIn(link) {
  only('open');
  const role = link.role?.name ?? 'the shared';
  $('open-text').textContent = `You are signed in. Opening this link gives you ${role} access to what it shares.`;
  $('open-button').textContent = `Open with ${role} access`;
  $('open-button').addEventListener('click', async () => {
    $('open-button').disabled = true;
    showMessage($('open-error'), '');
    const result = await call('POST', '/redeem', 'same-origin');
    if (result.ok) {
      location.assign(safeNext(result.data?.url, location.origin));
      return;
    }
    $('open-button').disabled = false;
    if (result.error === 'link_not_found') notWorking();
    else showMessage($('open-error'), messageFor(result.error));
  });
}

// ---- an anyone link ----

function showAnyone({ item }) {
  only('item');
  document.title = `${item.name} — ShareCraft`;
  $('title').textContent = item.name;
  if (item.kind === 'document') {
    $('kind').textContent = item.editor === 'text' ? 'Text document' : 'File';
    // A file no editor opens is previewed here when a browser can show it safely.
    $('file-tools').hidden = false;
    // A file never saved has nothing to download: "Not saved yet" takes the button's place.
    const offer = fileOffer(item.revised_at);
    $('download').hidden = !offer.download;
    $('not-saved').hidden = offer.download;
    $('download').href = contentUrl(item.id);
    $('download').setAttribute('aria-label', `Download ${item.name}`);
    if (item.editor === 'text') showText(item.id);
    else if (offer.download && previewable(item)) showPreview(item);
    return;
  }
  $('kind').textContent = 'Folder';
  trail = [{ id: item.id, name: item.name }];
  $('more').addEventListener('click', () => loadPage());
  openFolder(trail.length - 1);
}

function note(text) {
  showMessage($('error'), text);
}

/** Opens the folder at `at` in the trail, closing those below it. */
async function openFolder(at) {
  trail = trail.slice(0, at + 1);
  items = [];
  next = null;
  $('text-view').hidden = true;
  closePreview();
  showTrail();
  await loadPage();
}

function showTrail() {
  $('trail').hidden = trail.length < 2;
  $('trail').replaceChildren(
    h(
      'ol',
      { class: 'crumbs-line' },
      ...trail.map((step, at) => {
        if (at === trail.length - 1) return h('li', { 'aria-current': 'page' }, step.name);
        const button = h('button', { type: 'button', class: 'plain' }, step.name);
        button.addEventListener('click', () => openFolder(at));
        return h('li', {}, button);
      }),
    ),
  );
  $('title').textContent = trail[trail.length - 1].name;
}

async function loadPage() {
  const mine = ++loading;
  const folder = trail[trail.length - 1];
  const query = `limit=${PAGE}${next ? `&after=${encodeURIComponent(next)}` : ''}`;
  const result = await call('GET', `/children/${seg(folder.id)}?${query}`, 'omit');
  if (mine !== loading) return;
  if (!result.ok) {
    if (result.error === 'link_not_found') return notWorking();
    return note(messageFor(result.error));
  }
  note('');
  items = items.concat(result.data.items);
  next = result.data.next;
  $('items').hidden = items.length === 0;
  $('empty').hidden = items.length > 0;
  $('more').hidden = !next;
  $('items').replaceChildren(...items.map(row));
}

function row(child) {
  if (child.kind !== 'document') {
    const open = h('button', { type: 'button', class: 'plain', 'aria-label': `Open ${child.name}` }, child.name);
    open.addEventListener('click', () => {
      trail.push({ id: child.id, name: child.name });
      openFolder(trail.length - 1);
    });
    return h('li', {}, h('span', { class: 'grow' }, open), h('small', {}, 'Folder'));
  }
  const text = child.editor === 'text';
  const marks = itemMarks(child);
  // Held or quarantined content cannot be read, previewed or downloaded: only its mark shows.
  const held = Boolean(child.scan);
  const actions = [];
  if (held) {
    return h(
      'li',
      {},
      h('span', { class: 'grow' }, child.name),
      h('small', {}, text ? 'Text document' : 'File'),
      ...marks.map((mark) => h('small', { class: 'mark warn' }, mark)),
    );
  }
  if (!text && child.revised_at != null && previewable(child)) {
    const look = h('button', { type: 'button', class: 'plain', 'aria-label': `Preview ${child.name}` }, 'Preview');
    look.addEventListener('click', () => showPreview(child));
    actions.push(look);
  }
  if (text) {
    const view = h('button', { type: 'button', class: 'plain', 'aria-label': `Read ${child.name}` }, 'Read');
    view.addEventListener('click', () => showText(child.id));
    actions.push(view);
  }
  // A file never saved has nothing to download: "Not saved yet" takes the button's place.
  const offer = fileOffer(child.revised_at);
  actions.push(
    offer.download
      ? h('a', { class: 'button plain', href: contentUrl(child.id), 'aria-label': `Download ${child.name}` }, 'Download')
      : h('small', {}, offer.saved),
  );
  return h(
    'li',
    {},
    h('span', { class: 'grow' }, child.name),
    h('small', {}, text ? 'Text document' : 'File'),
    offer.download ? h('small', {}, offer.saved) : null,
    ...marks.map((mark) => h('small', { class: 'mark warn' }, mark)),
    h('span', { class: 'actions' }, ...actions),
  );
}

async function showText(id) {
  const result = await call('GET', `/text/${seg(id)}`, 'omit');
  if (!result.ok) {
    if (result.error === 'link_not_found') return notWorking();
    return note(messageFor(result.error));
  }
  note('');
  $('text-heading').textContent = result.data.name;
  $('text-body').textContent = result.data.revision === null ? '(This document has not been saved yet.)' : result.data.text;
  $('text-view').hidden = false;
  $('text-view').scrollIntoView?.({ block: 'nearest' });
}

// ---- previews of files no editor opens ----

/** Whether `item` is a file no editor opens that has a preview. */
function previewable(item) {
  return item.editor === 'other' && previewKindOf(item.name) !== null;
}

function closePreview() {
  previewing += 1;
  $('preview-view').hidden = true;
  $('preview-body').replaceChildren();
}

/**
 * Shows the preview of `item`: an image, a player, or text put in as text. Never a frame: the
 * file's bytes never run as a page.
 */
async function showPreview(item) {
  const mine = ++previewing;
  const kind = previewKindOf(item.name);
  const label = `Preview of ${item.name}`;
  $('preview-heading').textContent = item.name;
  $('preview-view').hidden = false;
  if (kind === 'text') {
    $('preview-body').replaceChildren(h('p', { class: 'meta' }, 'Loading the preview…'));
    let response;
    try {
      response = await fetch(previewUrl(item.id), { credentials: 'omit' });
    } catch {
      response = null;
    }
    if (mine !== previewing) return;
    if (!response?.ok) {
      if (response?.status === 404) return notWorking();
      $('preview-body').replaceChildren(h('p', {}, previewProblem(response?.status)));
      return;
    }
    const pre = h('pre', { class: 'text', 'aria-label': label });
    pre.textContent = await response.text();
    if (mine !== previewing) return;
    $('preview-body').replaceChildren(pre);
  } else {
    let media;
    if (kind === 'image') media = h('img', { src: previewUrl(item.id), alt: label, referrerpolicy: 'no-referrer' });
    else media = h(kind, { src: previewUrl(item.id), controls: true, preload: 'metadata', 'aria-label': label });
    media.addEventListener('error', () => {
      if (mine === previewing) $('preview-body').replaceChildren(h('p', {}, previewProblem(0)));
    });
    $('preview-body').replaceChildren(media);
  }
  $('preview-view').scrollIntoView?.({ block: 'nearest' });
}

function previewProblem(status) {
  if (status === 423) return messageFor('held');
  if (status === 415) return 'No preview for this type. Download the file to open it.';
  return 'The preview could not be shown. Download the file to open it.';
}
