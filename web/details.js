// The library's details pane: what a file or folder is (a large preview, its type, size, when
// and by whom it was last changed, who has access) without opening it. On wide screens it is a
// column beside the table that follows the selected row; on narrow ones a sheet that slides in
// from the right. The helpers at the top are pure and tested; `createDetails` draws the pane.

import {
  HELD_TEXT,
  can,
  formatDate,
  formatSize,
  h,
  itemActions,
  messageFor,
  pageFor,
  previewKindOf,
  principalParts,
  quarantineText,
  rolesText,
  seg,
} from './common.js';
import { EDITORS } from './editor-host.js';
import { fileIcon, fileKind, relativeTime, typeLabel } from './files.js';
import { isTextField } from './palette.js';

/** The width from which the pane is a column beside the table rather than a sheet. */
export const WIDE_QUERY = '(min-width: 1100px)';

/** How many lines of a text file the pane shows. */
export const TEXT_LINES = 30;

/**
 * What the pane's preview shows for `entry` (a child as `GET /api/nodes/{id}/children` lists
 * it): `folder` (a folder or library: its count instead), `quarantined` or `held` (the scanner's
 * message, never the bytes), `editor` (a file an editor opens: its icon and Open in …),
 * `empty` (a file with no version yet), `image`, `audio`, `video` or `text` (from the preview
 * route), or `none` (the type icon and Download).
 */
export function previewKind(entry) {
  if (!entry) return 'none';
  if (entry.kind === 'folder' || entry.kind === 'library') return 'folder';
  if (entry.scan === 'quarantined') return 'quarantined';
  if (entry.scan === 'held') return 'held';
  if (entry.editor && entry.editor !== 'other') return 'editor';
  if (!entry.revised_at) return 'empty';
  return previewKindOf(entry.name) ?? 'none';
}

/** The editor's name for a file an editor opens (`GridCraft`), or null. */
function editorName(entry) {
  return entry.editor && entry.editor !== 'other' && Object.hasOwn(EDITORS, entry.editor) ? EDITORS[entry.editor].name : null;
}

/** `12 items`, `1 item`, `Empty`, `500 or more items`, from `{ count, more }`. */
export function countText({ count, more = false }) {
  if (more) return `${count} or more items`;
  if (count === 0) return 'Empty';
  return count === 1 ? '1 item' : `${count} items`;
}

/**
 * A short account of `access` (`GET /api/nodes/{id}/access`): where it comes from, how many
 * people and groups have each role (`Owner 2 · Editor 1`), and the first `first` of them by name
 * with their role, with how many more there are.
 */
export function accessSummary(access, { first = 4 } = {}) {
  const assignments = Array.isArray(access?.assignments) ? access.assignments : [];
  const counts = new Map();
  for (const assignment of assignments) {
    const role = assignment.role?.name ?? 'Other';
    counts.set(role, (counts.get(role) ?? 0) + 1);
  }
  const total = assignments.length;
  const from = access?.inherits && access.source?.name ? `Shared like ${access.source.name}, the place above.` : 'Has its own permissions.';
  return {
    from,
    headline: total === 0 ? 'Nobody is given access here.' : total === 1 ? '1 person or group' : `${total} people and groups`,
    roles: [...counts].map(([role, count]) => `${role} ${count}`).join(' · '),
    names: assignments.slice(0, first).map((assignment) => ({
      name: principalParts(assignment.principal ?? {}, assignment.role).name,
      role: assignment.role?.name ?? '',
    })),
    more: Math.max(0, total - first),
  };
}

/**
 * The size of a file from the answer to a one-byte download (`Range: bytes=0-0`): the total of
 * `Content-Range` (`bytes 0-0/2048`, or `bytes *\/0` for an empty file) on a 206 or 416, the
 * `Content-Length` of a whole answer (200), otherwise null.
 */
export function sizeFromAnswer(status, contentRange, contentLength) {
  if (status === 206 || status === 416) {
    const match = /\/(\d+)\s*$/.exec(contentRange ?? '');
    return match ? Number(match[1]) : null;
  }
  if (status === 200 && contentLength != null && /^\d+$/.test(String(contentLength).trim())) return Number(contentLength);
  return null;
}

/** The first `lines` lines of `text`, and whether anything was left out. */
export function firstLines(text, lines = TEXT_LINES, { more = false } = {}) {
  const all = String(text ?? '').split(/\r\n|\n|\r/);
  if (all.length > lines) return { text: all.slice(0, lines).join('\n'), cut: true };
  return { text: all.join('\n'), cut: more };
}

/**
 * Who made the current version of a file, from its events (`GET /api/docs/{id}/events`): the
 * actor of the latest upload or creation within two minutes of `revisedAt`, named from `people`
 * (`{ id: name }`), `You` for `meId`. Null when no event matches or the person is unknown, so the
 * pane never names the wrong person.
 */
export function changedBy(events, revisedAt, people = {}, meId = null) {
  const at = revisedAt ? new Date(revisedAt).getTime() : NaN;
  if (!Array.isArray(events) || Number.isNaN(at)) return null;
  const made = events.filter((event) => ['upload.completed', 'document.created'].includes(event.kind) && event.actor && event.at);
  for (let index = made.length - 1; index >= 0; index -= 1) {
    const event = made[index];
    const when = new Date(event.at).getTime();
    if (Number.isNaN(when) || Math.abs(when - at) > 120_000) continue;
    if (meId && event.actor === meId) return 'You';
    return Object.hasOwn(people, event.actor) ? people[event.actor] : null;
  }
  return null;
}

/** People named in an access answer, as `{ id: name }` for `changedBy`. */
export function peopleIn(access) {
  const people = {};
  for (const assignment of access?.assignments ?? []) {
    const principal = assignment.principal;
    if (principal?.kind === 'user' && principal.id && principal.name) people[principal.id] = principal.name;
  }
  return people;
}

/**
 * What the pane shows for `entry`, given what has been fetched so far (`details`):
 * `size` (bytes; undefined while loading, null when unknown), `count` (`{ count, more }` for a
 * folder), `access` (the access answer; null when the person may not see it, `{ error }` when it
 * failed), `by` (who made the current version). Returns the preview kind, the facts as
 * `{ label, value, exact?, loading? }`, the held text, the access part, Open and the other
 * actions, each only where the person may.
 */
export function paneModel(entry, details = {}, { now = Date.now(), regional } = {}) {
  const kind = previewKind(entry);
  const folder = kind === 'folder';
  const editor = editorName(entry);
  const facts = [{ label: 'Type', value: typeLabel(entry) }];
  if (folder) {
    facts.push(details.count === undefined ? { label: 'Contains', value: 'Loading…', loading: true } : details.count ? { label: 'Contains', value: countText(details.count) } : null);
  } else {
    if (details.size === undefined && entry.revised_at && !['held', 'quarantined'].includes(kind)) {
      facts.push({ label: 'Size', value: 'Loading…', loading: true });
    } else if (typeof details.size === 'number') {
      facts.push({ label: 'Size', value: formatSize(details.size) });
    }
    if (entry.revised_at) {
      facts.push({
        label: 'Modified',
        value: relativeTime(entry.revised_at, now, regional),
        exact: formatDate(entry.revised_at, regional ?? {}),
        at: entry.revised_at,
      });
    } else {
      facts.push({ label: 'Modified', value: 'Not saved yet' });
    }
    if (details.by) facts.push({ label: 'Modified by', value: details.by });
  }
  if (entry.policy === 'blocked') facts.push({ label: 'Note', value: 'Type not allowed here' });

  let held = null;
  if (kind === 'quarantined') held = [quarantineText(entry.threat ?? null)];
  if (kind === 'held') held = [`${HELD_TEXT}.`, 'It can be previewed and opened as soon as the scanner has passed it.'];

  const allowed = itemActions(entry);
  let access;
  if (details.access === undefined) access = { state: 'loading' };
  else if (details.access === null) {
    access = {
      state: 'own',
      text: entry.access ? `You have ${rolesText(entry.access)} access.` : 'You have limited access: you can open only what was shared with you.',
      sharing: entry.inherits || entry.kind === 'library' ? 'Shared like the place above.' : 'Has its own permissions.',
    };
  } else if (details.access.error) access = { state: 'error', text: messageFor(details.access.error) };
  else access = { state: 'summary', ...accessSummary(details.access) };
  if (allowed.access) access.href = `/access.html?id=${seg(entry.id)}`;

  const open = {
    key: 'open',
    label: folder ? 'Open folder' : editor ? `Open in ${editor}` : 'Open',
    href: pageFor(entry.kind, entry.id, entry.editor),
  };
  const actions = [];
  const downloadable = !folder && entry.revised_at && !['held', 'quarantined'].includes(kind);
  if (downloadable) actions.push({ key: 'download', label: 'Download', href: `/api/docs/${seg(entry.id)}/content` });
  if (allowed.access) actions.push({ key: 'access', label: 'Share and access', href: `/access.html?id=${seg(entry.id)}` });
  if (allowed.rename) actions.push({ key: 'rename', label: 'Rename' });
  if (allowed.move) actions.push({ key: 'move', label: 'Move…' });
  if (allowed.remove) actions.push({ key: 'delete', label: 'Delete', danger: true });

  return {
    id: entry.id,
    name: entry.name,
    kind,
    icon: fileKind(entry.name, entry.editor, entry.kind),
    facts: facts.filter(Boolean),
    held,
    access,
    open,
    actions,
    // The sentence read out when the selection moves.
    announce: `${entry.name}, ${typeLabel(entry).toLowerCase()}.`,
  };
}

/**
 * Where the pane shows: 'column' beside the table on wide screens, 'sheet' on narrow ones, or
 * null. It shows only while it is open, the list is shown (`enabled`) and an item is selected:
 * with nothing selected there is no empty pane, and the table has the room.
 */
export function paneShown({ open = false, selected = false, wide = true, enabled = true } = {}) {
  if (!open || !enabled || !selected) return null;
  return wide ? 'column' : 'sheet';
}

/** Whether a key press toggles the pane: "i" alone, outside text fields, menus and other dialogs. */
export function isToggleKey(event, { sheetOpen = false } = {}) {
  if (event.key !== 'i' && event.key !== 'I') return false;
  if (event.ctrlKey || event.metaKey || event.altKey || event.defaultPrevented) return false;
  const target = event.target;
  if (isTextField(target)) return false;
  if (target?.closest?.('[role=menu]')) return false;
  const dialog = target?.closest?.('dialog');
  if (dialog && !(sheetOpen && dialog.classList.contains('details-sheet'))) return false;
  return true;
}

// ---- the pane on the page ----

/** A JSON answer from `/api{path}` that can be abandoned: `{ ok, status, data }`. */
async function getJson(path, signal) {
  const response = await fetch(`/api${path}`, { credentials: 'same-origin', signal });
  let data = null;
  try {
    data = await response.json();
  } catch {
    data = null;
  }
  return { ok: response.ok, status: response.status, data, error: response.ok ? null : data?.error ?? 'unknown' };
}

/**
 * Draws the details pane of the library page. `pane` is the `<aside>` beside the table, `sheet`
 * the `<dialog>` used below 1100 pixels, `status` a polite live region, `rows` the table body.
 * `entry(id)` finds a listed item; `act(key, item)` runs Rename, Move and Delete with the
 * page's own dialogs; `me` is the signed-in account. Returns what the page calls: `row(tr,
 * item)` for each row it draws, `redraw()` after it redraws the table, `show(item, options)`.
 */
export function createDetails({ pane, sheet, status, rows, page, entry, act, me, enabled = () => true }) {
  const wide = globalThis.matchMedia?.(WIDE_QUERY) ?? { matches: true, addEventListener() {} };
  let open = false; // the pane is shown (the column or the sheet)
  let selectedId = null;
  let details = {};
  let controller = null;
  let timer = null;
  let previewFor = null; // the id whose preview is in the pane
  let loadedFor = null; // the id whose details were asked for

  const heading = h('h2', { id: 'details-heading', tabindex: '-1' }, 'Details');
  const close = h('button', { type: 'button', class: 'plain details-close', 'aria-label': 'Close details' }, '×');
  const previewBox = h('div', { class: 'details-preview' });
  const info = h('div', { class: 'details-info' });
  const content = h(
    'div',
    { class: 'details-content' },
    h('div', { class: 'details-head' }, heading, h('span', { class: 'meta details-key', 'aria-hidden': 'true' }, 'Press i to hide'), close),
    previewBox,
    info,
  );
  close.addEventListener('click', () => hide());
  sheet.addEventListener('close', () => {
    if (open && !wide.matches) {
      open = false;
      afterHide();
    }
  });
  // A click on the backdrop (outside the sheet's box) closes it too.
  sheet.addEventListener('click', (event) => {
    if (event.target === sheet) sheet.close();
  });
  wide.addEventListener('change', () => {
    if (!open) return;
    if (wide.matches) {
      if (sheet.open) sheet.close();
      open = true;
    }
    place();
  });
  document.addEventListener('keydown', (event) => {
    if (!enabled() || !isToggleKey(event, { sheetOpen: sheet.open })) return;
    event.preventDefault();
    if (open && where()) return hide();
    const focusedRow = document.activeElement?.closest?.('#' + rows.id + ' tr');
    const id = focusedRow?.dataset.id ?? (selectedId && entry(selectedId) ? selectedId : rows.querySelector('tr')?.dataset.id);
    if (id) show(entry(id), { focus: !wide.matches });
  });
  rows.addEventListener('keydown', onRowsKey);
  rows.addEventListener('click', (event) => {
    const tr = event.target.closest('tr');
    if (!tr || !tr.dataset.id || event.target.closest('a,button,input,label,.menu-wrap')) return;
    tr.focus({ preventScroll: true });
    show(entry(tr.dataset.id));
  });

  try {
    if (localStorage.getItem('sharecraft-details') === 'open' && wide.matches) {
      open = true;
      place();
    }
  } catch {
    // no storage: the pane starts closed
  }

  function remember() {
    try {
      localStorage.setItem('sharecraft-details', open ? 'open' : 'closed');
    } catch {
      // not remembered
    }
  }

  /** Where the pane is now (see `paneShown`). */
  function where() {
    return paneShown({ open, selected: Boolean(selectedId && entry(selectedId)), wide: wide.matches, enabled: enabled() });
  }

  /** Puts the content in the column or the sheet, as the screen's width says; nothing selected, nowhere. */
  function place() {
    const at = where();
    if (at !== 'sheet' && sheet.open) sheet.close();
    pane.hidden = at !== 'column';
    page.classList.toggle('details-open', at === 'column');
    if (at === 'column') pane.replaceChildren(content);
    else if (at === 'sheet') {
      sheet.replaceChildren(content);
      if (!sheet.open) sheet.showModal();
    }
    draw();
  }

  /** Selects `item` and shows the pane. `focus` moves focus into the pane (from Details). */
  function show(item, { focus = false, delay = 0 } = {}) {
    if (!item) return;
    const shown = where();
    select(item, delay);
    if (!open || !shown) {
      open = true;
      remember();
      place();
      status.textContent = `Details of ${paneModel(item).announce}`;
      if (loadedFor !== item.id && delay === 0) load(item);
    }
    if (focus || !wide.matches) heading.focus();
  }

  function hide() {
    if (!open) return;
    open = false;
    remember();
    if (sheet.open) sheet.close();
    afterHide();
  }

  function afterHide() {
    remember();
    pane.hidden = true;
    page.classList.remove('details-open');
    // Focus goes back to the selected row when it was in the pane (or the sheet just closed).
    if (!document.activeElement || document.activeElement === document.body || content.contains(document.activeElement)) {
      rowOf(selectedId)?.focus();
    }
    status.textContent = 'Details hidden.';
  }

  function rowOf(id) {
    return id ? [...rows.querySelectorAll('tr[data-id]')].find((tr) => tr.dataset.id === id) ?? null : null;
  }

  /** Makes `item` the selected row and loads what the pane needs, after `delay` ms. */
  function select(item, delay = 0) {
    const changed = item.id !== selectedId;
    selectedId = item.id;
    markRows();
    if (!changed) return;
    controller?.abort();
    controller = null;
    clearTimeout(timer);
    details = {};
    previewFor = null;
    // Kept open with nothing selected, the pane comes back with the first selection.
    if (open && pane.hidden && !sheet.open) place();
    else draw();
    loadedFor = null;
    timer = setTimeout(() => {
      status.textContent = paneModel(item).announce;
      if (open && loadedFor !== item.id) load(item);
    }, delay);
  }

  /** The selected row is marked (aria-current) and is the table's one Tab stop. */
  function markRows() {
    const trs = [...rows.querySelectorAll('tr[data-id]')];
    const current = trs.find((tr) => tr.dataset.id === selectedId) ?? trs[0];
    for (const tr of trs) {
      tr.tabIndex = tr === current ? 0 : -1;
      if (tr.dataset.id === selectedId) tr.setAttribute('aria-current', 'true');
      else tr.removeAttribute('aria-current');
    }
  }

  function onRowsKey(event) {
    const tr = event.target.closest('tr[data-id]');
    if (!tr) return;
    if (event.target.closest('.menu-wrap')) return; // the Actions menu has its own keys
    if (event.altKey || event.ctrlKey || event.metaKey) return;
    if ((event.key === 'Enter' || event.key === ' ') && event.target === tr) {
      event.preventDefault();
      show(entry(tr.dataset.id), { focus: false });
      if (wide.matches) tr.focus();
      return;
    }
    const trs = [...rows.querySelectorAll('tr[data-id]')];
    const at = trs.indexOf(tr);
    let to = null;
    if (event.key === 'ArrowDown') to = trs[Math.min(trs.length - 1, at + 1)];
    else if (event.key === 'ArrowUp') to = trs[Math.max(0, at - 1)];
    else if ((event.key === 'Home' || event.key === 'End') && event.target === tr) to = event.key === 'Home' ? trs[0] : trs[trs.length - 1];
    if (!to) return;
    event.preventDefault();
    to.focus();
    const item = entry(to.dataset.id);
    if (item) select(item, 250);
  }

  /** Fetches what the pane shows for `item`; answers for an item no longer selected are dropped. */
  async function load(item) {
    controller?.abort();
    loadedFor = item.id;
    const abort = new AbortController();
    controller = abort;
    const kind = previewKind(item);
    const current = () => !abort.signal.aborted && selectedId === item.id;
    const update = (changes) => {
      if (!current()) return;
      details = { ...details, ...changes };
      draw();
    };
    if (open) drawPreview(item, kind, abort.signal);
    const tasks = [];
    if (kind === 'folder') {
      tasks.push(
        getJson(`/nodes/${seg(item.id)}/children?limit=500`, abort.signal).then((answer) =>
          update({ count: answer.ok ? { count: answer.data.items.length, more: Boolean(answer.data.next) } : null }),
        ),
      );
    } else if (item.revised_at && !['held', 'quarantined'].includes(kind)) {
      tasks.push(sizeOf(item, abort.signal).then((size) => update({ size })));
    } else {
      details.size = null;
    }
    if (can(item.access, 'manage_access')) {
      tasks.push(
        (async () => {
          const answer = await getJson(`/nodes/${seg(item.id)}/access`, abort.signal);
          update({ access: answer.ok ? answer.data : { error: answer.error } });
          if (!answer.ok || item.kind !== 'document' || !item.revised_at) return;
          const events = await getJson(`/docs/${seg(item.id)}/events`, abort.signal);
          if (events.ok) update({ by: changedBy(events.data, item.revised_at, peopleIn(answer.data), me?.id ?? null) });
        })(),
      );
    } else {
      details.access = null;
    }
    draw();
    await Promise.allSettled(tasks);
  }

  async function sizeOf(item, signal) {
    try {
      const response = await fetch(`/api/docs/${seg(item.id)}/content`, { credentials: 'same-origin', headers: { Range: 'bytes=0-0' }, signal });
      const size = sizeFromAnswer(response.status, response.headers.get('Content-Range'), response.headers.get('Content-Length'));
      response.body?.cancel().catch(() => {});
      return size;
    } catch {
      return null;
    }
  }

  /** The large preview of `item`, drawn once per selection. */
  function drawPreview(item, kind, signal) {
    if (previewFor === item.id) return;
    previewFor = item.id;
    const url = `/api/docs/${seg(item.id)}/preview`;
    const label = `Preview of ${item.name}`;
    const big = () => {
      const icon = fileIcon(fileKind(item.name, item.editor, item.kind));
      icon.setAttribute('width', '96');
      icon.setAttribute('height', '96');
      icon.classList.add('big');
      return h('div', { class: 'details-icon' }, icon);
    };
    const model = paneModel(item, details);
    if (kind === 'quarantined' || kind === 'held') {
      previewBox.replaceChildren(h('div', { class: 'details-held', role: 'note' }, ...model.held.map((line) => h('p', {}, line))));
    } else if (kind === 'image') {
      const img = h('img', { src: url, alt: label, decoding: 'async' });
      img.addEventListener('error', () => {
        if (previewFor === item.id) previewBox.replaceChildren(big(), h('p', { class: 'meta' }, 'No preview for this image.'));
      });
      previewBox.replaceChildren(h('div', { class: 'details-media' }, img));
    } else if (kind === 'audio' || kind === 'video') {
      const media = h(kind, { src: url, controls: true, preload: 'metadata', 'aria-label': label });
      media.addEventListener('error', () => {
        if (previewFor === item.id) previewBox.replaceChildren(big(), h('p', { class: 'meta' }, 'No preview for this file.'));
      });
      previewBox.replaceChildren(h('div', { class: 'details-media' }, media));
    } else if (kind === 'text') {
      previewBox.replaceChildren(big(), h('p', { class: 'meta' }, 'Loading the first lines…'));
      textPreview(item, url, signal);
    } else if (kind === 'editor') {
      const button = h('a', { class: 'button', href: model.open.href }, model.open.label);
      previewBox.replaceChildren(big(), button);
    } else if (kind === 'folder') {
      previewBox.replaceChildren(big());
    } else {
      previewBox.replaceChildren(big(), h('p', { class: 'meta' }, kind === 'empty' ? 'Not saved yet.' : 'No preview for this type.'));
    }
  }

  // The first lines of a text file, read as text (never as markup) and then left unread.
  async function textPreview(item, url, signal) {
    const shown = (...nodes) => {
      if (previewFor === item.id && !signal.aborted) previewBox.replaceChildren(...nodes.filter(Boolean));
    };
    try {
      const response = await fetch(url, { credentials: 'same-origin', signal });
      if (response.status === 423) return shown(h('div', { class: 'details-held', role: 'note' }, h('p', {}, `${HELD_TEXT}.`)));
      if (!response.ok || !response.body) return shown(h('p', { class: 'meta' }, response.status === 415 ? 'No preview for this file.' : messageFor('unavailable')));
      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      let text = '';
      let more = true;
      while (text.length < 8192 && text.split('\n').length <= TEXT_LINES) {
        const { done, value } = await reader.read();
        if (done) {
          more = false;
          break;
        }
        text += decoder.decode(value, { stream: true });
      }
      if (more) reader.cancel().catch(() => {});
      const lines = firstLines(text.slice(0, 8192), TEXT_LINES, { more });
      const pre = h('pre', { class: 'details-text', tabindex: '0', 'aria-label': `First lines of ${item.name}` });
      pre.textContent = lines.text;
      shown(pre, lines.cut ? h('p', { class: 'meta' }, 'Only the first lines are shown. Open the file to see all of it.') : null);
    } catch {
      // abandoned (another item was chosen) or no connection: the icon stays
    }
  }

  /** Redraws the facts, access and actions of the selected item (the preview stays). */
  function draw() {
    if (!open) return;
    const item = selectedId ? entry(selectedId) : null;
    if (!item) {
      previewFor = null;
      previewBox.replaceChildren();
      info.replaceChildren(h('p', { class: 'meta details-empty' }, 'Choose a file or folder to see its details: click its row, or move to it with the arrow keys.'));
      return;
    }
    const model = paneModel(item, details);
    if (previewFor !== item.id && controller) drawPreview(item, model.kind, controller.signal);
    const facts = h(
      'dl',
      { class: 'details-facts' },
      ...model.facts.flatMap((fact) => [
        h('dt', {}, fact.label),
        h(
          'dd',
          { 'aria-busy': fact.loading ? 'true' : null },
          fact.at ? h('time', { datetime: fact.at }, fact.value) : fact.value,
          fact.exact ? h('small', { class: 'meta' }, fact.exact) : null,
        ),
      ]),
    );
    info.replaceChildren(h('h3', { class: 'details-name' }, model.name), facts, accessPart(model.access), actionsPart(item, model));
  }

  function accessPart(access) {
    const parts = [];
    if (access.state === 'loading') parts.push(h('p', { class: 'meta', 'aria-busy': 'true' }, 'Loading…'));
    else if (access.state === 'error') parts.push(h('p', { class: 'meta' }, access.text));
    else if (access.state === 'own') parts.push(h('p', {}, access.sharing), h('p', { class: 'meta' }, access.text));
    else {
      parts.push(h('p', {}, h('strong', {}, access.headline), access.roles ? ` · ${access.roles}` : ''), h('p', { class: 'meta' }, access.from));
      if (access.names.length) {
        parts.push(
          h(
            'ul',
            { class: 'details-people' },
            ...access.names.map((person) => h('li', {}, h('span', {}, person.name), h('small', { class: 'meta' }, person.role))),
          ),
        );
      }
      if (access.more) parts.push(h('p', { class: 'meta' }, `and ${access.more} more`));
    }
    if (access.href) parts.push(h('p', {}, h('a', { href: access.href }, 'See and change who has access')));
    return h('section', { class: 'details-access', 'aria-labelledby': 'details-access-heading' }, h('h4', { id: 'details-access-heading' }, 'Who has access'), ...parts);
  }

  function actionsPart(item, model) {
    const nodes = [];
    if (model.kind !== 'editor') nodes.push(h('a', { class: 'button', href: model.open.href }, model.open.label));
    for (const action of model.actions) {
      if (action.href) {
        nodes.push(h('a', { class: 'button plain', href: action.href, download: action.key === 'download' ? '' : null }, action.label));
        continue;
      }
      const button = h('button', { type: 'button', class: action.danger ? 'plain danger' : 'plain' }, action.label);
      button.addEventListener('click', () => {
        if (sheet.open) {
          open = false;
          sheet.close();
          pane.hidden = true;
        }
        act(action.key, item);
      });
      nodes.push(button);
    }
    return h('div', { class: 'details-actions', role: 'group', 'aria-label': `Actions for ${item.name}` }, ...nodes);
  }

  return {
    /** Gives a drawn row what selecting needs: its id, a tab stop of its own, its label. */
    row(tr, item) {
      tr.dataset.id = item.id;
      tr.tabIndex = -1;
      tr.classList.add('pickable');
    },
    /** After the table is drawn again: the marks, and the pane when its item changed or went. */
    redraw() {
      if (selectedId && !entry(selectedId)) {
        controller?.abort();
        selectedId = null;
        details = {};
      }
      markRows();
      if (!enabled() && open) {
        pane.hidden = true;
        page.classList.remove('details-open');
        if (sheet.open) sheet.close();
        return;
      }
      if (open) place();
    },
    show,
    hide,
  };
}
