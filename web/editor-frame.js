// The focused editor frame (file.html and doc.html): one slim bar above the editor with the way
// back (the site's mark), the file's name (a button that renames it, where allowed) over its
// breadcrumb, the save status as a quiet dot and word, who else is in the file, Share, Download,
// Save and a "⋯" menu with the rest (Details, Activity, …); and a Focus mode that hides the bar and leaves only a small
// "Exit focus" pill, remembered per viewer.
//
// `presenceModel`, `liveLook`, `liveStatus`, `downloadNote`, `statusLabel`, `backLabel` and
// `isFocusShortcut` are pure and tested;
// `setupEditorFrame` needs the page. The site's look colours the bar only (siteChrome does that).

import { initials, avatarColour } from './account.js';
import { api, breadcrumbParts, h, messageFor, policyRefusalText, seg } from './common.js';
import { setupMenu } from './menu.js';
import { addShortcut, shortcuts } from './shortcuts.js';

export const FOCUS_KEY = 'sharecraft-editor-focus';
const NARROW = '(max-width: 760px)';

/**
 * Who else is in the session, for the bar: `people` as the page's live connection lists them
 * (`{ user?, id?, name, me?, can_edit? }`), `me` this person's account id (or null). This person
 * is left out (every tab of theirs), each other person once (by account, else by name), in the
 * order listed. Answers `{ shown: [{ key, name, initials, colour, readOnly }], more, total,
 * label }`: at most `limit` shown, `more` the rest, `label` every name in plain words ("" when
 * nobody else is here).
 */
export function presenceModel(people, me, limit = 4) {
  const seen = new Set();
  const others = [];
  for (const person of Array.isArray(people) ? people : []) {
    // A live place whose connection dropped is kept for a while, but not shown (Proposal 7).
    if (!person || person.me === true || person.connected === false) continue;
    const user = typeof person.user === 'string' ? person.user : typeof person.id === 'string' ? person.id : '';
    if (me && user === me) continue;
    const name = String(person.name ?? '').trim() || 'Someone';
    const key = user ? `u:${user}` : `n:${name}`;
    if (seen.has(key)) continue;
    seen.add(key);
    others.push({ key, name, initials: initials(name), colour: avatarColour(user || name), readOnly: person.can_edit === false });
  }
  const cap = Math.max(1, Math.floor(limit) || 1);
  const shown = others.length > cap ? others.slice(0, cap) : others;
  const names = others.map((person) => (person.readOnly ? `${person.name} (read only)` : person.name));
  return { shown, more: others.length - shown.length, total: others.length, label: names.length ? `Also here: ${listText(names)}` : '' };
}

/**
 * How a person in a live session looks in the editor's canvas: the initials and the light-theme
 * colour of the avatar the bar gives the same account (`#rrggbb`), so a selection and its face
 * match. The name stands in for a missing account.
 */
export function liveLook(person) {
  const name = String(person?.name ?? '');
  const user = typeof person?.user === 'string' ? person.user : '';
  return { initials: initials(name), colour: avatarColour(user || name).light.bg };
}

const CONNECTED = new Set(['live']);
const AWAY = new Set(['offline', 'closed', 'stalled', 'idle']);

/**
 * The bar's status for a live document (Proposal 14): `state` is the editor's last live state
 * (`phase`, `waiting`, `canEdit`) with what the page knows besides: `paused` (the room paused
 * changes), `saving` (a checkpoint upload is under way) and `unsaved` (this tab made changes
 * since the last stored version, which the server has taken) and `stopped` (the page stopped
 * for good and does not connect again). `waiting` counts changes the server has not taken yet:
 * only those are "kept in this tab". Answers a state for statusLabel.
 */
export function liveStatus(state) {
  const { phase = '', waiting = 0, canEdit = true, paused = false, saving = false, unsaved = false, stopped = false } = state ?? {};
  if (canEdit === false) return { tone: 'view' };
  if (stopped) {
    // Nothing is sent again: changes still waiting here are lost when the page goes.
    if (waiting > 0) return { tone: 'failed', text: "Changes made here since the connection stopped couldn't be sent." };
    if (unsaved) return { tone: 'unsaved', text: 'Your changes are kept on the server.' };
    return { tone: 'saved' };
  }
  if (!CONNECTED.has(phase)) {
    if (AWAY.has(phase) && waiting > 0) {
      return { tone: 'offline', text: 'Your changes are kept in this tab and are sent when the connection is back.' };
    }
    return { tone: 'connecting' };
  }
  if (paused) {
    return { tone: 'unsaved', text: 'Changes are paused until the latest version is saved.' };
  }
  if (saving || waiting > 0) return { tone: 'saving' };
  if (unsaved) {
    return { tone: 'unsaved', text: 'Your changes are kept on the server and go into the stored file shortly.' };
  }
  return { tone: 'saved' };
}

const BEHIND = "The stored file doesn't include the latest changes yet";

/**
 * What the Download link says about a live document's stored file: `doc` is the document
 * answer (`live.behind`); `since` what this page saw after it: `savedSince` (a newer version
 * was stored) and `unsaved` (changes not yet in a stored version). '' when the file is current.
 */
export function downloadNote(doc, { savedSince = false, unsaved = false } = {}) {
  const behind = doc?.live?.behind === true && !savedSince;
  return behind || unsaved ? BEHIND : '';
}

function listText(names) {
  if (names.length < 2) return names.join('');
  return `${names.slice(0, -1).join(', ')} and ${names[names.length - 1]}`;
}

const WORDS = Object.freeze({
  saved: 'Saved',
  saving: 'Saving…',
  connecting: 'Connecting…',
  offline: 'Offline — changes kept',
  view: 'Read only',
  failed: 'Not saved',
  notice: 'Unsaved changes',
  unsaved: 'Unsaved changes kept',
});

/**
 * The quiet status word for a state of the frame: `state` is a tone (`saved`, `saving`,
 * `connecting`, `offline`, `view`, `failed`, `notice`) or `{ tone, text, word }`, `text` the
 * page's longer sentence and `word` a word that replaces the tone's own. Answers `{ tone, word,
 * detail }`, `detail` the sentence when it says more than the word ('' otherwise).
 */
export function statusLabel(state) {
  const { tone = '', text = '', word = '' } = typeof state === 'string' ? { tone: state } : state ?? {};
  const shown = word || (Object.hasOwn(WORDS, tone) ? WORDS[tone] : '') || text || '';
  const detail = text && text !== shown ? text : '';
  return { tone, word: shown, detail };
}

/**
 * The accessible name of the way back: "Back to Documents in Home" for the folder or library the
 * file is in and its site, "Back to Home" when that is the site, "Back to sites" without a place.
 */
export function backLabel(folder, site) {
  if (!folder?.name) return 'Back to sites';
  if (!site?.name || folder.id === site.id) return `Back to ${folder.name}`;
  return `Back to ${folder.name} in ${site.name}`;
}

/** Whether a key event is the Focus shortcut: Ctrl+Shift+F (Cmd+Shift+F on a Mac). */
export function isFocusShortcut(event) {
  return Boolean(event && (event.ctrlKey || event.metaKey) && event.shiftKey && !event.altKey && (event.key === 'F' || event.key === 'f' || event.code === 'KeyF'));
}

function readFocus() {
  try {
    return globalThis.localStorage?.getItem(FOCUS_KEY) === '1';
  } catch {
    return false;
  }
}

function writeFocus(on) {
  try {
    if (on) globalThis.localStorage?.setItem(FOCUS_KEY, '1');
    else globalThis.localStorage?.removeItem(FOCUS_KEY);
  } catch {
    // Not remembered: the choice holds for this page load.
  }
}

/**
 * Wires the slim bar of an editor page. `frame` is the `.frame` element. Options:
 * `details()` answers the facts for the Details dialog (`[{ label, value }]`), `onRenamed(node)`
 * is called with the server's answer after a rename, `onActivity({ opener })` opens the file's
 * Activity panel (offered to those who may share the file). Returns the controls the page drives:
 * `setName(name)`, `setRenamable(yes)`, `setPlace(path, folder)`, `setStatus(state)`,
 * `setPresence(people, me)`, `setNodeId(id)`.
 */
export function setupEditorFrame(frame, { details = () => [], onRenamed = () => {}, onActivity = null } = {}) {
  const $ = (id) => document.getElementById(id);
  const bar = frame.querySelector('.bar');
  const title = $('title');
  const back = $('back');
  const statusBox = frame.querySelector('.status');
  const statusWord = $('status');
  const presence = $('presence');
  const accessLink = $('access-link');
  const download = $('download');
  const focusButton = $('focus-toggle');
  const exitButton = $('focus-exit');
  const focusStatus = $('focus-status');
  const more = $('more');
  const moreMenu = $('more-menu');
  const narrow = globalThis.matchMedia?.(NARROW);

  let name = title.textContent;
  let renamable = false;
  let nodeId = '';
  let path = [];

  // ---- the name, and renaming ----

  const drawName = () => {
    if (renamable) {
      const button = h('button', { type: 'button', class: 'doc-name', title: 'Rename', 'aria-label': `${name}, rename` }, name);
      button.addEventListener('click', askRename);
      title.replaceChildren(button);
    } else {
      title.replaceChildren(name);
    }
  };

  const renameInput = h('input', { type: 'text', id: 'rename-name', maxlength: '255', autocomplete: 'off', required: true, 'aria-describedby': 'rename-hint' });
  const renameHint = h('p', { id: 'rename-hint', class: 'meta' });
  const renameError = h('p', { class: 'msg', role: 'alert' });
  const renameCancel = h('button', { type: 'button', class: 'plain' }, 'Cancel');
  const renameForm = h(
    'form',
    { method: 'dialog' },
    h('h2', { id: 'rename-title' }, 'Rename'),
    h('div', { class: 'field' }, h('label', { for: 'rename-name' }, 'Name'), renameInput),
    renameHint,
    renameError,
    h('div', { class: 'row end' }, renameCancel, h('button', { type: 'submit' }, 'Rename')),
  );
  const renameDialog = h('dialog', { 'aria-labelledby': 'rename-title' }, renameForm);
  document.body.append(renameDialog);
  renameCancel.addEventListener('click', () => renameDialog.close());
  renameDialog.addEventListener('close', () => title.querySelector('button')?.focus());

  function askRename() {
    const extension = /\.[A-Za-z0-9]{1,10}$/.exec(name)?.[0] ?? '';
    renameHint.textContent = extension ? `Keep the ending ${extension}, which opens the file in its editor.` : '';
    renameError.textContent = '';
    renameInput.value = name;
    renameDialog.showModal();
    renameInput.setSelectionRange(0, name.length - extension.length);
    renameInput.focus();
  }

  renameForm.addEventListener('submit', async (event) => {
    event.preventDefault();
    const wanted = renameInput.value.trim();
    if (!wanted) {
      renameError.textContent = 'Give the file a name.';
      return;
    }
    if (wanted === name) {
      renameDialog.close();
      return;
    }
    const result = await api('PATCH', `/nodes/${seg(nodeId)}`, { name: wanted });
    if (!result.ok) {
      renameError.textContent = policyRefusalText(result.error, result.data, wanted) ?? messageFor(result.error);
      return;
    }
    renameDialog.close();
    onRenamed(result.data);
  });

  // ---- Details ----

  const detailsList = h('dl', { class: 'facts' });
  const detailsClose = h('button', { type: 'button' }, 'Close');
  const detailsDialog = h(
    'dialog',
    { 'aria-labelledby': 'details-title' },
    h('h2', { id: 'details-title' }, 'Details'),
    detailsList,
    h('div', { class: 'row end' }, detailsClose),
  );
  document.body.append(detailsDialog);
  detailsClose.addEventListener('click', () => detailsDialog.close());
  detailsDialog.addEventListener('close', () => more.focus());

  const showDetails = () => {
    const facts = [...details(), { label: 'Location', value: path.map((step) => step.name).join(' › ') || '—' }];
    detailsList.replaceChildren(
      ...facts.filter((fact) => fact?.value).flatMap((fact) => [h('dt', {}, fact.label), h('dd', {}, fact.value)]),
    );
    detailsDialog.showModal();
    detailsClose.focus();
  };

  // ---- the "⋯" menu ----

  const entry = (label, run, attributes = {}) => {
    const item = h('button', { type: 'button', role: 'menuitem', ...attributes }, label);
    item.addEventListener('click', run);
    return item;
  };
  const shareItem = h('a', { role: 'menuitem', href: '#' }, 'Share');
  const downloadItem = h('a', { role: 'menuitem', href: '#', download: '' }, 'Download');
  const focusItem = entry('Focus mode', () => setFocus(true), { 'aria-keyshortcuts': 'Control+Shift+F' });
  const narrowOnly = [h('li', { role: 'none' }, shareItem), h('li', { role: 'none' }, downloadItem), h('li', { role: 'none' }, focusItem)];
  const locationLabel = h('li', { role: 'none', class: 'menu-label', 'aria-hidden': 'true' }, 'Location');
  const locationGroup = h('ul', { role: 'group', class: 'menu-group', 'aria-label': 'Location' });
  const locationItems = [locationLabel, h('li', { role: 'none' }, locationGroup)];
  // Activity (activity.js): who changed the file and when. The events route answers only those
  // who manage access, as Share does, so the item shows when Share would.
  const activityItem = h('li', { role: 'none', hidden: true }, entry('Activity', () => onActivity?.({ opener: more })));
  moreMenu.replaceChildren(
    h('li', { role: 'none' }, entry('Details', showDetails)),
    activityItem,
    ...narrowOnly,
    h('li', { role: 'separator', class: 'menu-separator' }),
    ...locationItems,
  );

  // What the menu holds follows the bar each time it opens: on a narrow screen Share, Download
  // and Focus move into it, and so does the location (the breadcrumb is hidden there).
  const syncMenu = () => {
    const small = Boolean(narrow?.matches);
    activityItem.hidden = !onActivity || accessLink.hidden;
    narrowOnly[0].hidden = !small || accessLink.hidden;
    shareItem.href = accessLink.getAttribute('href') ?? '#';
    narrowOnly[1].hidden = !small || download.hidden || !download.getAttribute('href');
    downloadItem.href = download.getAttribute('href') ?? '#';
    narrowOnly[2].hidden = !small;
    const parts = breadcrumbParts(path, { linkLast: false });
    locationGroup.replaceChildren(
      ...parts.map((part) =>
        h('li', { role: 'none' }, part.href ? h('a', { role: 'menuitem', href: part.href }, part.name) : h('span', { class: 'menu-text' }, part.name)),
      ),
    );
    const showLocation = small && parts.length > 0;
    for (const item of locationItems) item.hidden = !showLocation;
    moreMenu.querySelector('.menu-separator').hidden = !showLocation && narrowOnly.every((item) => item.hidden);
  };
  more.addEventListener('click', syncMenu, true);
  more.addEventListener('keydown', syncMenu, true);
  setupMenu(more, moreMenu);

  // ---- Focus mode ----

  const setFocus = (on, { remember = true } = {}) => {
    frame.classList.toggle('focus', on);
    bar.hidden = on;
    exitButton.hidden = !on;
    focusButton.setAttribute('aria-pressed', String(on));
    focusStatus.textContent = on ? statusWord.textContent : '';
    if (!remember) return;
    writeFocus(on);
    // Focus that was in the bar goes to the pill, and back to the bar after.
    const active = document.activeElement;
    if (on && (!active || active === document.body || bar.contains(active))) exitButton.focus();
    else if (!on && (!active || active === exitButton || active === document.body)) (narrow?.matches ? more : focusButton).focus();
  };
  focusButton.addEventListener('click', () => setFocus(true));
  exitButton.addEventListener('click', () => setFocus(false));
  // Works in fields and dialogs too, as it always has; `isFocusShortcut` also takes the F key of
  // layouts where Shift gives another letter.
  addShortcut({
    keys: ['Mod+Shift+F'],
    label: 'Focus mode: hide the bar, or show it again',
    scope: 'editor',
    inFields: true,
    inMenus: true,
    inDialogs: true,
    frames: true,
    stop: true,
    when: (event) => isFocusShortcut(event),
    run: () => setFocus(!frame.classList.contains('focus')),
  });
  // An editor runs in a frame on this origin: the shortcut works there too. Editors drawn on a
  // canvas take every key for themselves, so the frame listens first (capture).
  frame.addEventListener(
    'load',
    (event) => {
      if (event.target?.tagName !== 'IFRAME') return;
      try {
        shortcuts.listen(event.target.contentWindow, { frames: true });
      } catch {
        // Not reachable: the shortcut works outside the editor only.
      }
    },
    true,
  );
  setFocus(readFocus(), { remember: false });

  // ---- status and presence ----

  const setStatus = (state) => {
    const label = statusLabel(state);
    if (statusWord.textContent !== label.word) statusWord.textContent = label.word;
    statusBox.title = label.detail || label.word;
    if (frame.classList.contains('focus') && focusStatus.textContent !== label.word) focusStatus.textContent = label.word;
    return label;
  };

  const setPresence = (people, me) => {
    // Up to four faces, two on a narrow screen, then "+N".
    const model = presenceModel(people, me, narrow?.matches ? 2 : 4);
    presence.hidden = model.total === 0;
    presence.setAttribute('aria-label', model.label);
    presence.title = model.label;
    const faces = model.shown.map((person) => {
      const avatar = h('span', { class: person.readOnly ? 'avatar small read-only' : 'avatar small', 'aria-hidden': 'true' }, person.initials);
      avatar.style.setProperty('--avatar-light-bg', person.colour.light.bg);
      avatar.style.setProperty('--avatar-light-ink', person.colour.light.ink);
      avatar.style.setProperty('--avatar-dark-bg', person.colour.dark.bg);
      avatar.style.setProperty('--avatar-dark-ink', person.colour.dark.ink);
      return avatar;
    });
    if (model.more > 0) faces.push(h('span', { class: 'avatar small more', 'aria-hidden': 'true' }, `+${model.more}`));
    presence.replaceChildren(...faces);
  };

  return {
    setName(next) {
      name = String(next ?? '');
      drawName();
    },
    setRenamable(yes) {
      renamable = Boolean(yes) && nodeId !== '';
      drawName();
      if (!renamable && renameDialog.open) renameDialog.close();
    },
    setNodeId(id) {
      nodeId = id;
    },
    setPlace(nextPath, folder) {
      path = Array.isArray(nextPath) ? nextPath : [];
      const label = backLabel(folder, path[0]);
      back.setAttribute('aria-label', label);
      back.title = label;
    },
    setStatus,
    setPresence,
  };
}
