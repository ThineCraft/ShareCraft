// The notifications inbox (round 23): a bell in the account bar, between search and the avatar,
// with a count of what is new, opening a panel (a popover on wide screens, a bottom sheet on
// phones, where the shell's Inbox tab carries the count) with two parts: "Needs your decision"
// (open requests for access on items the person manages: approve with a role from a small menu,
// or decline with an optional reply, both in place, with a toast) and "Your requests" (the
// person's own requests that someone decided: approved with the role and a link to the item, or
// declined with the reply).
//
// What is new: anything that arrived (an open request) or was decided (one of yours) after the
// last time this person opened the inbox in this browser (a timestamp in localStorage; storage
// that cannot be used just means everything counts as new). "Mark all as read" clears the marks.
//
// Both lists come from one shared fetch: on page load, when the tab is shown again, and every
// two minutes while it is visible (never while hidden).
//
// The server lists only decided requests under /api/requests/mine (an open request never says
// that an item exists), so "waiting" is shown only if a request without a decision ever comes.
//
// The helpers at the top are pure and tested (inbox.test.mjs); `setupInbox` needs a page.

import { REQUEST_MAX, api, formatDate, h, messageFor, pageFor, requestDecisionText, requestProblem, rolesWithin, seg } from './common.js';
import { avatarColour, initials } from './account.js';
import { fileIcon, fileKind, relativeTime } from './files.js';
import { setupMenu } from './menu.js';
import { dragToClose, phoneShellNow } from './sheet.js';
import { showToast } from './toast.js';

/** How often the lists are fetched again while the tab is visible. */
export const REFRESH_EVERY = 2 * 60_000;
/** Showing the tab again fetches the lists unless they were fetched less than this long ago. */
export const REFRESH_ON_SHOW_AFTER = 15_000;

const KIND_WORDS = Object.freeze({ site: 'site', library: 'library', folder: 'folder', document: 'file' });

const time = (iso) => {
  const at = Date.parse(iso ?? '');
  return Number.isNaN(at) ? null : at;
};

/**
 * One request in words, for either list. An open request (`GET /api/requests`: it has a
 * `requester`) or one of the person's own (`GET /api/requests/mine`: it has a `decision`, or
 * none while it waits). Answers `{ id, section: 'decide'|'yours', status: 'open'|'waiting'|
 * 'approved'|'declined', at, person, name, kind, icon, where, href, message, reply, role,
 * headline, statusText, label }`: `at` is when it last changed (asked, or decided), `where` the
 * place above the item ("Home › Documents"), `href` the item's page when the person can open
 * it, `label` one sentence for screen readers.
 */
export function describeRequest(request) {
  const node = request.node ?? null;
  const path = Array.isArray(request.path) ? request.path : [];
  const name = node?.name || 'an item you cannot see';
  const kind = node?.kind ?? 'document';
  const where = path.map((step) => step.name).join(' › ');
  const place = where ? ` in ${where}` : '';
  const kindWord = node ? KIND_WORDS[kind] ?? 'item' : 'item';
  const href = node?.access ? pageFor(kind, node.id) : null;
  const base = {
    id: request.id,
    name,
    kind,
    icon: kind === 'site' ? 'folder' : fileKind(name, null, kind),
    where,
    href,
    message: request.message?.trim() || '',
    reply: request.reply?.trim() || '',
  };
  if (request.requester) {
    const who = request.requester.name || 'Someone';
    const headline = `${who} asked for access`;
    return {
      ...base,
      section: 'decide',
      status: 'open',
      at: request.created_at ?? null,
      person: { id: request.requester.id, name: who },
      role: null,
      headline,
      statusText: 'Needs your decision',
      label: `${who} asked for access to the ${kindWord} ${name}${place}.${base.message ? ` Their message: ${base.message}` : ''}`,
    };
  }
  const decision = request.decision === 'approved' || request.decision === 'declined' ? request.decision : 'waiting';
  const role = request.role?.name ?? null;
  const statusText = decision === 'waiting' ? 'Waiting for a decision' : requestDecisionText(request);
  const headline =
    decision === 'approved'
      ? role
        ? `You can now open ${name} as ${role}`
        : `You can now open ${name}`
      : decision === 'declined'
        ? `Your request for ${name} was declined`
        : `You asked for access to ${name}`;
  return {
    ...base,
    section: 'yours',
    status: decision,
    at: decision === 'waiting' ? request.created_at ?? null : request.decided_at ?? request.created_at ?? null,
    person: null,
    role,
    href: decision === 'approved' ? href : null,
    headline,
    statusText,
    label: `${headline}${place}.${base.reply ? ` Their reply: ${base.reply}` : ''}`,
  };
}

/** Whether `item` (from describeRequest) changed after `lastSeen` (ms, or null for never opened). */
const isNewSince = (item, lastSeen) => {
  if (item.status === 'waiting') return false; // nothing has happened to it yet
  if (lastSeen == null) return true;
  const at = time(item.at);
  return at !== null && at > lastSeen;
};

/**
 * The inbox from the two lists and the time the person last opened it (ms, or null): `{ decide,
 * yours, unread, newest }`. `decide` and `yours` are describeRequest items, newest first, each
 * with `isNew`; `newest` is the latest time in either list (ms, or null).
 */
export function inboxModel(open = [], mine = [], lastSeen = null) {
  const describe = (list) =>
    (Array.isArray(list) ? list : [])
      .map(describeRequest)
      .map((item) => ({ ...item, isNew: isNewSince(item, lastSeen) }))
      .sort((a, b) => (time(b.at) ?? 0) - (time(a.at) ?? 0));
  const decide = describe(open);
  const yours = describe(mine);
  const times = [...decide, ...yours].map((item) => time(item.at)).filter((at) => at !== null);
  const model = { decide, yours, newest: times.length ? Math.max(...times) : null };
  return { ...model, unread: unreadCount(model) };
}

/** How many items of an inbox model are new. */
export function unreadCount(model) {
  return [...(model?.decide ?? []), ...(model?.yours ?? [])].filter((item) => item.isNew).length;
}

/** The bell's accessible name: "Notifications" or "Notifications, 2 new". */
export const bellLabel = (count) => (count > 0 ? `Notifications, ${count} new` : 'Notifications');

/** The badge text: the count, "99+" above 99, '' for none. */
export const badgeText = (count) => (count > 99 ? '99+' : count > 0 ? String(count) : '');

/**
 * The time to remember as "seen" when the inbox is opened or marked read at `now`: never earlier
 * than the newest item, so an item stamped by a server clock ahead of this one does not stay new.
 */
export const nextSeen = (model, now) => Math.max(now, model?.newest ?? 0);

/** Whether showing the tab again should fetch the lists (last fetched at `lastAt`, ms). */
export const refreshOnShow = (lastAt, now) => lastAt == null || now - lastAt >= REFRESH_ON_SHOW_AFTER;

/** The localStorage key of the time `userId` last opened the inbox. */
export const seenKey = (userId) => `sharecraft-inbox-seen:${userId ?? 'anyone'}`;

/** The time the person last opened the inbox (ms), or null: never, or storage cannot be read. */
export function readSeen(userId, store = storage()) {
  try {
    const value = Number(store?.getItem(seenKey(userId)));
    return Number.isFinite(value) && value > 0 ? value : null;
  } catch {
    return null;
  }
}

/** Remembers `at` (ms) as when the person last opened the inbox; never throws. */
export function writeSeen(userId, at, store = storage()) {
  try {
    store?.setItem(seenKey(userId), String(Math.round(at)));
  } catch {
    // a private window or blocked storage: the marks come back next time, nothing worse
  }
}

function storage() {
  try {
    return globalThis.localStorage ?? null;
  } catch {
    return null;
  }
}

// ---- the page part ----

const SVG = 'http://www.w3.org/2000/svg';
const BELL = ['M6 16.5V11a6 6 0 0 1 12 0v5.5l1.5 2h-15z', 'M10 20.5a2 2 0 0 0 4 0'];

/** The bell's path data (24 by 24, strokes), shared with the shell's Inbox tab. */
export const BELL_ICON = Object.freeze(BELL);

function strokeIcon(paths, className) {
  const svg = document.createElementNS(SVG, 'svg');
  for (const [name, value] of Object.entries({ viewBox: '0 0 24 24', width: 22, height: 22, 'aria-hidden': 'true', focusable: 'false', class: className })) {
    svg.setAttribute(name, String(value));
  }
  for (const d of paths) {
    const path = document.createElementNS(SVG, 'path');
    path.setAttribute('d', d);
    svg.append(path);
  }
  return svg;
}

function personAvatar(person) {
  const colour = avatarColour(person.id || person.name);
  const element = h('span', { class: 'avatar small', 'aria-hidden': 'true' }, initials(person.name));
  element.style.setProperty('--avatar-light-bg', colour.light.bg);
  element.style.setProperty('--avatar-light-ink', colour.light.ink);
  element.style.setProperty('--avatar-dark-bg', colour.dark.bg);
  element.style.setProperty('--avatar-dark-ink', colour.dark.ink);
  return element;
}

let state = null;

/** The current count, painted on the bell and the shell's Inbox tab (which may come later). */
export function paintCounts() {
  if (!state) return;
  const count = state.count;
  for (const badge of document.querySelectorAll('.inbox-count')) {
    badge.textContent = badgeText(count);
    badge.hidden = count === 0;
  }
  state.bell.setAttribute('aria-label', bellLabel(count));
  const tab = document.querySelector('.tabbar [data-tab=inbox]');
  tab?.setAttribute('aria-label', count > 0 ? `Inbox, ${count} new` : 'Inbox');
}

/** Opens the inbox from `from` (the bell, or the shell's Inbox tab). */
export function openInbox(from) {
  state?.open(from);
}

/** Adds the bell to the account bar `bar` for the signed-in person `me` and starts fetching. */
export function setupInbox(me, { bar = document.querySelector('.topbar') } = {}) {
  if (state || typeof document === 'undefined' || !bar) return;
  const badge = () => h('span', { class: 'inbox-count', 'aria-hidden': 'true', hidden: true });
  const bell = h(
    'button',
    { type: 'button', id: 'inbox-button', class: 'ghost bell-button', 'aria-label': 'Notifications', 'aria-haspopup': 'dialog', 'aria-expanded': 'false', 'aria-controls': 'inbox-panel' },
    strokeIcon(BELL, 'bell-svg'),
    badge(),
  );
  const accountWrap = bar.querySelector('.account-wrap');
  if (accountWrap) accountWrap.before(bell);
  else bar.append(bell);

  const announcer = h('p', { class: 'visually-hidden', role: 'status', 'aria-live': 'polite' });
  document.body.append(announcer);

  let data = { open: [], mine: [] };
  let loaded = false;
  let failed = false;
  let lastAt = null;
  let inFlight = null;
  let shownSeen = null; // what "new" means in the panel while it is open
  let roles = null;
  let opener = bell;
  let stopDrag = null;
  const decided = new Set(); // ids decided here, kept out until the server agrees

  state = { bell, count: 0, open: (from) => show(from) };

  // ---- the panel ----
  const title = h('h2', { id: 'inbox-title', tabindex: '-1' }, 'Notifications');
  const markRead = h('button', { type: 'button', class: 'ghost small inbox-mark' }, 'Mark all as read');
  const close = h('button', { type: 'button', class: 'ghost small icon inbox-close', 'aria-label': 'Close' }, '×');
  const status = h('p', { class: 'meta inbox-status', role: 'status', 'aria-live': 'polite' });
  const decideTitle = h('h3', { id: 'inbox-decide-title', tabindex: '-1' }, 'Needs your decision');
  const decideList = h('ul', { class: 'inbox-list', 'aria-labelledby': 'inbox-decide-title' });
  const decideEmpty = h('p', { class: 'meta inbox-empty' }, 'Nothing is waiting for your decision.');
  const yoursTitle = h('h3', { id: 'inbox-yours-title' }, 'Your requests');
  const yoursList = h('ul', { class: 'inbox-list', 'aria-labelledby': 'inbox-yours-title' });
  const yoursEmpty = h('p', { class: 'meta inbox-empty' }, 'When someone decides a request for access you made, the answer shows here.');
  const panel = h(
    'dialog',
    { id: 'inbox-panel', class: 'inbox', 'aria-labelledby': 'inbox-title' },
    h('div', { class: 'sheet-handle', 'aria-hidden': 'true' }),
    h('div', { class: 'inbox-head' }, title, markRead, close),
    status,
    h('section', { class: 'inbox-section' }, decideTitle, decideList, decideEmpty),
    h('section', { class: 'inbox-section' }, yoursTitle, yoursList, yoursEmpty),
    h('p', { class: 'inbox-foot' }, h('a', { href: '/requests.html' }, 'All access requests')),
  );
  document.body.append(panel);

  const isOpen = () => panel.open;

  function model() {
    const open = data.open.filter((request) => !decided.has(request.id));
    return inboxModel(open, data.mine, isOpen() ? shownSeen : readSeen(me.id));
  }

  function setCount(count) {
    const before = state.count;
    state.count = count;
    paintCounts();
    if (count > before && loaded && !isOpen()) announcer.textContent = count === 1 ? 'One new notification.' : `${count} new notifications.`;
  }

  function render() {
    const now = model();
    decideList.replaceChildren(...now.decide.map(decideRow));
    yoursList.replaceChildren(...now.yours.map(yoursRow));
    decideList.hidden = now.decide.length === 0;
    decideEmpty.hidden = now.decide.length > 0;
    yoursList.hidden = now.yours.length === 0;
    yoursEmpty.hidden = now.yours.length > 0;
    markRead.disabled = now.unread === 0;
    status.textContent = failed ? 'Could not load your notifications. ShareCraft tries again in a moment.' : loaded ? '' : 'Loading…';
    decideTitle.textContent = now.decide.length ? `Needs your decision (${now.decide.length})` : 'Needs your decision';
  }

  function whenLine(item, verb) {
    const iso = item.at;
    return h(
      'p',
      { class: 'meta inbox-when' },
      item.isNew ? h('span', { class: 'inbox-new' }, 'New') : null,
      h('time', { datetime: iso ?? '', title: iso ? formatDate(iso) : '' }, `${verb} ${iso ? relativeTime(iso) : ''}`.trim()),
    );
  }

  function placeLine(item, { icon = true } = {}) {
    const name = item.href ? h('a', { href: item.href, class: 'inbox-item-name' }, item.name) : h('span', { class: 'inbox-item-name' }, item.name);
    return h(
      'p',
      { class: 'inbox-place' },
      icon ? h('span', { class: 'inbox-icon', 'aria-hidden': 'true' }, fileIcon(item.icon)) : null,
      h('span', { class: 'inbox-place-text' }, name, item.where ? h('small', { class: 'inbox-where' }, item.where) : null),
    );
  }

  function decideRow(item) {
    const request = data.open.find((entry) => entry.id === item.id);
    const problem = h('p', { class: 'msg inbox-problem', role: 'alert' });
    const choices = rolesWithin(roles ?? [], request?.node?.access);
    const approve = h('button', { type: 'button', id: `inbox-approve-${item.id}`, class: 'small menu-button inbox-approve' }, 'Approve', h('span', { 'aria-hidden': 'true' }, ' ▾'));
    approve.setAttribute('aria-label', `Approve ${item.person.name}'s request`);
    const roleMenu = h(
      'ul',
      { id: `inbox-roles-${item.id}`, class: 'menu inbox-roles', 'aria-label': `Give ${item.person.name}` },
      ...choices.map((role) => {
        const choice = h('button', { type: 'button', role: 'menuitem' }, `As ${role.name}`);
        choice.addEventListener('click', () => decide(item, row, 'approve', { role: role.id }, role.name));
        return h('li', { role: 'none' }, choice);
      }),
    );
    const wrap = h('div', { class: 'menu-wrap' }, approve, roleMenu);
    if (roles && choices.length === 0) {
      approve.disabled = true;
      approve.title = 'There is no role you can give here.';
    }
    if (roles === null) approve.disabled = true;
    else setupMenu(approve, roleMenu);

    const declineButton = h('button', { type: 'button', class: 'secondary small inbox-decline' }, 'Decline');
    const reply = h('input', { type: 'text', id: `inbox-reply-${item.id}`, maxlength: REQUEST_MAX, autocomplete: 'off' });
    const sendDecline = h('button', { type: 'submit', class: 'danger small' }, 'Decline request');
    const cancel = h('button', { type: 'button', class: 'ghost small' }, 'Cancel');
    const form = h(
      'form',
      { class: 'inbox-decline-form', hidden: true },
      h('label', { for: reply.id }, `Reply to ${item.person.name} (optional)`),
      reply,
      h('div', { class: 'inbox-actions' }, sendDecline, cancel),
    );
    const actions = h('div', { class: 'inbox-actions' }, wrap, declineButton);
    declineButton.addEventListener('click', () => {
      actions.hidden = true;
      form.hidden = false;
      reply.focus();
    });
    cancel.addEventListener('click', () => {
      form.hidden = true;
      actions.hidden = false;
      problem.textContent = '';
      declineButton.focus();
    });
    form.addEventListener('submit', (event) => {
      event.preventDefault();
      const bad = requestProblem(reply.value);
      if (bad) {
        problem.textContent = messageFor(bad);
        return;
      }
      const text = reply.value.trim();
      decide(item, row, 'decline', text ? { reply: text } : {});
    });

    const row = h(
      'li',
      { class: item.isNew ? 'inbox-item is-new' : 'inbox-item', 'data-id': item.id, 'aria-label': item.label },
      personAvatar(item.person),
      h(
        'div',
        { class: 'inbox-body' },
        h('p', { class: 'inbox-headline' }, h('strong', {}, item.person.name), ' asked for access to'),
        placeLine(item),
        item.message ? h('p', { class: 'quote inbox-message' }, item.message) : null,
        whenLine(item, 'Asked'),
        actions,
        form,
        problem,
      ),
    );
    return row;
  }

  function yoursRow(item) {
    const pill = h('span', { class: `inbox-status-pill ${item.status}` }, item.statusText);
    return h(
      'li',
      { class: item.isNew ? 'inbox-item is-new' : 'inbox-item', 'data-id': item.id, 'aria-label': item.label },
      h('span', { class: 'inbox-icon big', 'aria-hidden': 'true' }, fileIcon(item.icon)),
      h(
        'div',
        { class: 'inbox-body' },
        h('p', { class: 'inbox-headline' }, pill),
        placeLine(item, { icon: false }),
        item.reply ? h('p', { class: 'quote inbox-message' }, h('span', { class: 'meta' }, 'Their reply: '), item.reply) : null,
        whenLine(item, item.status === 'waiting' ? 'Asked' : 'Decided'),
        item.status === 'approved' && item.href ? h('p', { class: 'inbox-open' }, h('a', { href: item.href, class: 'button secondary small' }, `Open ${item.name}`)) : null,
      ),
    );
  }

  async function decide(item, row, how, body, roleName) {
    const problem = row.querySelector('.inbox-problem');
    problem.textContent = '';
    const buttons = [...row.querySelectorAll('button')];
    for (const button of buttons) button.disabled = true;
    const result = await api('POST', `/requests/${seg(item.id)}/${how}`, body);
    if (result.ok) {
      decided.add(item.id);
      const nextId = (row.nextElementSibling ?? row.previousElementSibling)?.dataset.id;
      row.remove();
      const message =
        how === 'approve'
          ? `${item.person.name} can now open ${item.name} as ${roleName}.`
          : `Declined ${item.person.name}'s request for ${item.name}.`;
      showToast(message, { kind: 'ok' });
      render();
      const target = nextId ? decideList.querySelector(`[data-id="${CSS.escape(nextId)}"] .inbox-approve`) : null;
      (target && !target.disabled ? target : decideTitle).focus();
      load();
      return;
    }
    for (const button of buttons) button.disabled = false;
    if (result.error === 'already_decided') {
      showToast('Someone has already decided that request.', { kind: 'error' });
      decided.add(item.id);
      row.remove();
      render();
      decideTitle.focus();
      load();
      return;
    }
    problem.textContent = messageFor(result.error);
  }

  // ---- opening and closing ----
  function show(from = bell) {
    if (isOpen()) {
      hide(true);
      return;
    }
    opener = from ?? bell;
    shownSeen = readSeen(me.id);
    const phone = phoneShellNow();
    panel.classList.toggle('shell-sheet', phone);
    panel.classList.toggle('as-popover', !phone);
    if (!phone) {
      const box = bell.getBoundingClientRect();
      panel.style.setProperty('--inbox-top', `${Math.round(box.bottom + 6)}px`);
    }
    render();
    for (const control of [bell, opener]) control.setAttribute('aria-expanded', 'true');
    if (phone) {
      panel.showModal();
      stopDrag = dragToClose(panel, () => hide(true), { handle: panel.querySelector('.sheet-handle') });
    } else {
      panel.show();
    }
    title.focus();
    markSeen();
    if (roles === null) loadRoles();
    if (refreshOnShow(lastAt, Date.now())) load();
  }

  function hide(refocus) {
    if (!isOpen()) return;
    panel.close();
    afterClose(refocus);
  }

  function afterClose(refocus) {
    stopDrag?.();
    stopDrag = null;
    for (const control of [bell, opener]) control.setAttribute('aria-expanded', 'false');
    shownSeen = null;
    if (refocus) (opener?.isConnected && opener.offsetParent !== null ? opener : bell).focus();
  }

  // The modal sheet closes on Escape by itself; the popover closes here.
  panel.addEventListener('cancel', (event) => {
    event.preventDefault();
    hide(true);
  });
  panel.addEventListener('keydown', (event) => {
    if (event.key === 'Escape' && !event.defaultPrevented && panel.classList.contains('as-popover')) {
      event.preventDefault();
      hide(true);
    }
  });
  // A press outside the popover (or on the dim page above the sheet) closes it.
  document.addEventListener(
    'pointerdown',
    (event) => {
      if (!isOpen()) return;
      const target = event.target;
      if (target === bell || bell.contains(target) || (opener && opener.contains(target))) return;
      if (target.closest?.('.menu.as-sheet, .toasts')) return;
      if (panel.classList.contains('shell-sheet')) {
        if (target !== panel) return;
        const box = panel.getBoundingClientRect();
        if (event.clientY >= box.top) return;
      } else if (panel.contains(target)) return;
      hide(false);
    },
    true,
  );
  bell.addEventListener('click', () => show(bell));
  close.addEventListener('click', () => hide(true));
  markRead.addEventListener('click', () => {
    const at = nextSeen(model(), Date.now());
    writeSeen(me.id, at);
    shownSeen = at;
    render();
    status.textContent = 'Everything is marked as read.';
    title.focus();
  });

  function markSeen() {
    const now = model();
    writeSeen(me.id, nextSeen(now, Date.now()));
    setCount(0);
  }

  // ---- fetching ----
  async function loadRoles() {
    const result = await api('GET', '/roles');
    roles = result.ok && Array.isArray(result.data) ? result.data : [];
    if (isOpen()) keepFocus(render);
  }

  /** Re-draws the panel, putting focus back on the same control when it was inside a row. */
  function keepFocus(draw) {
    const active = document.activeElement;
    const rowId = active?.closest?.('.inbox-item')?.dataset.id;
    const which = active?.classList?.contains('inbox-decline') ? '.inbox-decline' : active?.classList?.contains('inbox-approve') ? '.inbox-approve' : null;
    if (active?.closest?.('.inbox-decline-form:not([hidden])')) return; // typing a reply: leave it
    draw();
    if (rowId && which) panel.querySelector(`[data-id="${CSS.escape(rowId)}"] ${which}`)?.focus();
  }

  function load() {
    if (inFlight) return inFlight;
    lastAt = Date.now();
    inFlight = Promise.all([api('GET', '/requests'), api('GET', '/requests/mine')])
      .then(([open, mine]) => {
        failed = !open.ok && !mine.ok;
        data = { open: open.ok && Array.isArray(open.data) ? open.data : data.open, mine: mine.ok && Array.isArray(mine.data) ? mine.data : data.mine };
        for (const id of [...decided]) if (!data.open.some((request) => request.id === id)) decided.delete(id);
        loaded = true;
        if (isOpen()) {
          writeSeen(me.id, nextSeen(model(), Date.now()));
          keepFocus(render);
          setCount(0);
        } else {
          setCount(unreadCount(model()));
        }
      })
      .finally(() => {
        inFlight = null;
      });
    return inFlight;
  }

  let timer = 0;
  const schedule = () => {
    clearInterval(timer);
    timer = 0;
    if (document.visibilityState === 'visible') timer = setInterval(load, REFRESH_EVERY);
  };
  document.addEventListener('visibilitychange', () => {
    if (document.visibilityState === 'visible' && refreshOnShow(lastAt, Date.now())) load();
    schedule();
  });
  if (document.visibilityState === 'visible') load();
  schedule();
}
