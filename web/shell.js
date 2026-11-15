// The phone-first shell (round 20). Below 700 pixels wide, or with a finger below 900, pages
// with the account bar get a tab bar at the bottom, in thumb reach: Home (the start page), Search
// (the command palette as a full-screen sheet), Recent (the files opened lately, from start.js's
// store, as a bottom sheet), Inbox (round 23: inbox.js's notifications as a bottom sheet, with
// the count of new ones on the tab; the bell in the top bar steps aside) and You (the account menu as a bottom sheet). The account bar at the
// top keeps the wordmark and the page's one context action (marked data-shell-action, such as a
// library's New), which moves up there. The tab bar steps aside while the on-screen keyboard is
// up. A long press (half a second, a finger held still) on a library row or card opens its
// Actions sheet; an ordinary tap still opens the item, and a scroll never presses.
//
// Editor pages (file.html, doc.html) keep their slim bar: they do not load this module.
// The decisions are pure (sheet.js, and `currentTab` here); `setupShell` wires them to the page.

import { formatDate, h, pageFor } from './common.js';
import { fileIcon, fileKind, relativeTime } from './files.js';
import { createLongPress, keyboardUp, LONG_PRESS_MS, dragToClose, phoneShellNow } from './sheet.js';
import { readRecent } from './start.js';

/** The tabs, in order: key, label and the icon's path data (24 by 24, drawn as strokes). */
export const SHELL_TABS = Object.freeze([
  { key: 'home', label: 'Home', icon: ['M3 11.5 12 4l9 7.5', 'M5.5 9.5V20h4.5v-5.5h4V20h4.5V9.5'] },
  { key: 'search', label: 'Search', icon: ['M10.5 4a6.5 6.5 0 1 1 0 13 6.5 6.5 0 0 1 0-13z', 'm15.5 15.5 4.5 4.5'] },
  { key: 'recent', label: 'Recent', icon: ['M12 3.5a8.5 8.5 0 1 1 0 17 8.5 8.5 0 0 1 0-17z', 'M12 7.5V12l3 2'] },
  { key: 'inbox', label: 'Inbox', icon: ['M6 16.5V11a6 6 0 0 1 12 0v5.5l1.5 2h-15z', 'M10 20.5a2 2 0 0 0 4 0'] },
  { key: 'you', label: 'You', icon: ['M12 4a4 4 0 1 1 0 8 4 4 0 0 1 0-8z', 'M4.5 20.5c.8-3.6 3.8-5.5 7.5-5.5s6.7 1.9 7.5 5.5'] },
]);

/** The tab that is the page at `pathname`: 'home' on the start page, otherwise none (null). */
export function currentTab(pathname = '') {
  return pathname === '/' || pathname === '/index.html' ? 'home' : null;
}

/** What a recent file shows in the Recent sheet: `{ href, name, kind, detail }`. */
export function recentRow(file, now = Date.now()) {
  return {
    href: pageFor('document', file.id, file.editor),
    name: file.name,
    kind: fileKind(file.name, file.editor),
    detail: [file.site, relativeTime(file.at, now)].filter(Boolean).join(' · '),
  };
}

// ---- the page part ----

const SVG = 'http://www.w3.org/2000/svg';

function icon(paths) {
  const svg = document.createElementNS(SVG, 'svg');
  for (const [name, value] of Object.entries({ viewBox: '0 0 24 24', width: 24, height: 24, 'aria-hidden': 'true', focusable: 'false', class: 'tab-svg' })) {
    svg.setAttribute(name, String(value));
  }
  for (const d of paths) {
    const path = document.createElementNS(SVG, 'path');
    path.setAttribute('d', d);
    svg.append(path);
  }
  return svg;
}

let installed = false;

/**
 * Adds the shell to a page with the account bar for the signed-in person `me`. `account` is the
 * account menu's controls (`open(where, from)`), from setupAccountMenu.
 */
export function setupShell(me, { account } = {}) {
  if (installed || typeof document === 'undefined') return;
  installed = true;
  const root = document.documentElement;
  const here = currentTab(location.pathname);

  // ---- the tab bar ----
  const tab = ({ key, label, icon: paths }) => {
    const inner = [h('span', { class: 'tab-icon' }, icon(paths)), h('span', { class: 'tab-label' }, label)];
    if (key === 'home') return h('a', { href: '/', class: 'tab', 'data-tab': key, 'aria-current': here === 'home' ? 'page' : null }, ...inner);
    const attrs = { type: 'button', class: 'tab', 'data-tab': key, 'aria-expanded': 'false' };
    if (key === 'search') Object.assign(attrs, { 'aria-haspopup': 'dialog', 'aria-label': 'Search or jump to' });
    if (key === 'recent') Object.assign(attrs, { 'aria-haspopup': 'dialog', 'aria-label': 'Recently opened files' });
    if (key === 'inbox') {
      Object.assign(attrs, { 'aria-haspopup': 'dialog', 'aria-label': 'Inbox', 'aria-controls': 'inbox-panel' });
      inner[0].append(h('span', { class: 'inbox-count', 'aria-hidden': 'true', hidden: true }));
    }
    if (key === 'you') Object.assign(attrs, { 'aria-haspopup': 'menu', 'aria-controls': 'account-menu', 'aria-label': `You: ${me.name || me.email || 'your account'}` });
    return h('button', attrs, ...inner);
  };
  const tabs = SHELL_TABS.map(tab);
  const bar = h('nav', { class: 'tabbar', 'aria-label': 'Main' }, h('ul', {}, ...tabs.map((item) => h('li', {}, item))));
  document.body.append(bar);
  const [, searchTab, recentTab, inboxTab, youTab] = tabs;

  searchTab.addEventListener('click', () => {
    import('./palette.js').then((palette) => palette.openPalette(searchTab)).catch(() => {});
  });
  youTab.addEventListener('click', () => {
    if (youTab.getAttribute('aria-expanded') === 'true') account?.close(true);
    else account?.open('first', youTab);
  });
  inboxTab.addEventListener('click', () => {
    import('./inbox.js').then((inbox) => inbox.openInbox(inboxTab)).catch(() => {});
  });
  import('./inbox.js').then((inbox) => inbox.paintCounts()).catch(() => {});
  const recent = recentSheet(me, recentTab);
  recentTab.addEventListener('click', () => recent.open());

  // ---- the context action moves up into the account bar ----
  const topbar = document.querySelector('.topbar');
  const actions = [...document.querySelectorAll('[data-shell-action]')].map((element) => ({ element, mark: document.createComment('shell action') }));
  const placeActions = (phone) => {
    for (const action of actions) {
      const inBar = topbar?.contains(action.element);
      if (phone && topbar && !inBar) {
        action.element.before(action.mark);
        const accountWrap = topbar.querySelector('.account-wrap');
        if (accountWrap) accountWrap.before(action.element);
        else topbar.append(action.element);
        action.element.classList.add('in-topbar');
      } else if (!phone && inBar && action.mark.isConnected) {
        action.mark.replaceWith(action.element);
        action.element.classList.remove('in-topbar');
      }
    }
  };

  // ---- on or off, and the keyboard ----
  const viewport = globalThis.visualViewport;
  const update = () => {
    const phone = phoneShellNow();
    root.classList.toggle('phone-shell', phone);
    placeActions(phone);
    const up = phone && viewport ? keyboardUp(innerHeight, viewport.height) : false;
    root.classList.toggle('keyboard-up', up);
    if (viewport) root.style.setProperty('--viewport-height', `${Math.round(viewport.height)}px`);
  };
  update();
  addEventListener('resize', update);
  viewport?.addEventListener('resize', update);

  setupLongPress(root);
}

/** The Recent sheet: a modal bottom sheet listing the files opened lately in this browser. */
function recentSheet(me, opener) {
  let dialog = null;
  let list;
  let empty;
  let stopDrag = null;

  const build = () => {
    const handle = h('div', { class: 'sheet-handle', 'aria-hidden': 'true' });
    const close = h('button', { type: 'button', class: 'ghost sheet-close' }, 'Close');
    close.addEventListener('click', () => dialog.close());
    list = h('ul', { class: 'shell-recent', 'aria-labelledby': 'recent-sheet-title' });
    empty = h('p', { class: 'meta shell-empty' }, 'Files you open appear here, newest first.');
    dialog = h(
      'dialog',
      { class: 'shell-sheet', 'aria-labelledby': 'recent-sheet-title' },
      handle,
      h('div', { class: 'sheet-head' }, h('h2', { id: 'recent-sheet-title' }, 'Recently opened'), close),
      list,
      empty,
    );
    document.body.append(dialog);
    dialog.addEventListener('close', () => {
      stopDrag?.();
      stopDrag = null;
      opener.setAttribute('aria-expanded', 'false');
      opener.focus();
    });
    // A press on the dimmed page above the sheet (the dialog itself, outside its box) closes it.
    dialog.addEventListener('click', (event) => {
      if (event.target !== dialog) return;
      const box = dialog.getBoundingClientRect();
      if (event.clientY < box.top || event.clientX < box.left || event.clientX > box.right) dialog.close();
    });
  };

  const open = () => {
    if (!dialog) build();
    if (dialog.open) return;
    const files = readRecent(me.id);
    list.replaceChildren(
      ...files.map((file) => {
        const row = recentRow(file);
        const iso = new Date(file.at).toISOString();
        return h(
          'li',
          {},
          h(
            'a',
            { href: row.href, class: 'shell-recent-link' },
            fileIcon(row.kind),
            h('span', { class: 'shell-recent-text' }, h('span', { class: 'shell-recent-name' }, row.name), h('small', { title: formatDate(iso) }, row.detail)),
          ),
        );
      }),
    );
    list.hidden = files.length === 0;
    empty.hidden = files.length > 0;
    opener.setAttribute('aria-expanded', 'true');
    dialog.showModal();
    (list.querySelector('a') ?? dialog.querySelector('.sheet-close')).focus();
    stopDrag = dragToClose(dialog, () => dialog.close(), { handle: dialog.querySelector('.sheet-handle') });
  };
  return { open };
}

/** What a long press may land on: a row or card that has an Actions menu, with the menu's button. */
function pressable(target) {
  if (!(target instanceof Element)) return null;
  if (target.closest('button, input, select, textarea, label, .menu, dialog, [contenteditable]')) return null;
  const holder = target.closest('.files tbody tr, .library-card');
  const button = holder?.querySelector('.menu-button.dots');
  return button ? { holder, button } : null;
}

/** Long press (touch or pen, in the phone shell) on a library row or card opens its Actions sheet. */
function setupLongPress(root) {
  const press = createLongPress();
  const clock = () => performance.now();
  let current = null; // { holder, button }
  let timer = 0;
  let quietUntil = 0;

  const end = () => {
    clearTimeout(timer);
    current?.holder.classList.remove('long-pressing');
    current = null;
  };
  const fire = () => {
    if (!current) return;
    const { button } = current;
    end();
    button.click();
    quietUntil = Date.now() + 1500; // until the finger is lifted (see pointerup)
  };
  const point = (event) => ({ x: event.clientX, y: event.clientY, pointerId: event.pointerId, button: event.button });

  document.addEventListener(
    'pointerdown',
    (event) => {
      if (!root.classList.contains('phone-shell') || event.pointerType === 'mouse') return;
      const found = pressable(event.target);
      if (!found || press.down(point(event), clock()) !== 'waiting') return;
      current = found;
      found.holder.classList.add('long-pressing');
      timer = setTimeout(() => {
        if (press.tick(clock()) === 'pressed') fire();
      }, LONG_PRESS_MS);
    },
    { passive: true },
  );
  document.addEventListener(
    'pointermove',
    (event) => {
      const outcome = press.move(point(event), clock());
      if (outcome === 'cancelled') end();
      else if (outcome === 'pressed') fire();
    },
    { passive: true },
  );
  document.addEventListener('pointerup', (event) => {
    const outcome = press.up(point(event), clock());
    if (outcome === 'pressed') fire();
    if (outcome === 'pressed' || outcome === 'after-press') quietUntil = Date.now() + 400; // the click of this lift
    end();
  });
  const cancel = () => {
    press.cancel();
    end();
  };
  document.addEventListener('pointercancel', cancel);
  document.addEventListener('scroll', cancel, { capture: true, passive: true });
  // The lift after a long press is not a tap on the row (it would open the file).
  document.addEventListener(
    'click',
    (event) => {
      if (Date.now() >= quietUntil || event.target.closest?.('.menu')) return;
      quietUntil = 0;
      event.preventDefault();
      event.stopPropagation();
    },
    true,
  );
  // No browser menu (or text selection callout) on a row held down in the shell.
  document.addEventListener(
    'contextmenu',
    (event) => {
      if (root.classList.contains('phone-shell') && pressable(event.target)) event.preventDefault();
    },
    true,
  );
}
