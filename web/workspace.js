// The workspace layout of site pages: one left sidebar that holds the site's own links (when the
// site's look puts its navigation in a column) and, on a library, the folder tree beneath them.
// From 1000 pixels the sidebar is a column that folds to a slim rail (remembered per viewer);
// below that it is a drawer, a modal dialog opened from a Menu or Folders button.
//
// The pure part, `layoutFor`, decides; `sidebar(page)` builds and runs the page part. This module
// builds its few elements itself (no import from common.js, which imports it).

/** Below this width the sidebar is a drawer. */
export const DRAWER_BELOW = 1000;
/** Where the viewer's choice (column or rail) is kept. */
export const SIDEBAR_KEY = 'sharecraft-sidebar';

/**
 * How the sidebar is shown at `width` pixels: `{ side, label }`. `side` is 'none' (nothing to
 * hold: horizontal navigation and no folders), 'drawer' (narrow screens), 'rail' (folded by the
 * viewer, `prefs.collapsed`) or 'column'. `siteNav` is the site's navigation, 'vertical' or
 * 'horizontal'; `folders` says the page has a folder tree. `label` names the sidebar: 'Menu' when
 * it holds the site's links, 'Folders' when it holds only the tree.
 */
export function layoutFor(width, prefs = {}, siteNav = 'horizontal', { folders = false } = {}) {
  const links = siteNav === 'vertical';
  if (!links && !folders) return { side: 'none', label: null };
  const label = links ? 'Menu' : 'Folders';
  if (!(width >= DRAWER_BELOW)) return { side: 'drawer', label };
  return { side: prefs.collapsed ? 'rail' : 'column', label };
}

/** The viewer's stored choice as prefs: `{ collapsed }`; anything but 'rail' is open. */
export function readPrefs(text) {
  return { collapsed: text === 'rail' };
}

/** What is stored for `prefs`. */
export function prefsText(prefs) {
  return prefs?.collapsed ? 'rail' : 'column';
}

// ---- the page part ----

const controllers = new WeakMap();
let drawers = 0;

function el(tag, attrs = {}, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(attrs)) {
    if (value != null) node.setAttribute(key, value);
  }
  for (const child of children) if (child != null) node.append(child);
  return node;
}

function storedPrefs() {
  try {
    return readPrefs(localStorage.getItem(SIDEBAR_KEY));
  } catch {
    return readPrefs(null);
  }
}

/** The node after which the workspace goes: the navigation bar, the site header or the account bar. */
function anchorOf(page) {
  return (
    page.querySelector(':scope > [data-chrome]') ??
    page.querySelector(':scope > [data-site-header]') ??
    page.querySelector(':scope > .topbar') ??
    null
  );
}

/** The page's sidebar, built the first time it is asked for (see the controller below). */
export function sidebar(page) {
  return controllers.get(page) ?? build(page);
}

/** The page's sidebar when it has one, or null. */
export function existingSidebar(page) {
  return controllers.get(page) ?? null;
}

/**
 * Wraps what follows the page's bars in `.ws`: the sidebar `.side` and the rest in `.side-main`.
 * The controller answers `links(view)` (the site's navigation at the top of the sidebar, or null),
 * `folders(view)` (the folder tree beneath), `opener(button)` (the page's own Menu or Folders
 * button instead of the one drawn at the top of the main column), `layout()` and `dissolve()`.
 */
function build(page) {
  drawers += 1;
  const n = drawers;
  const body = el('div', { class: 'side-body', id: `side-body-${n}` });
  const toggle = el(
    'button',
    { type: 'button', class: 'plain side-toggle', 'aria-controls': body.id, 'aria-expanded': 'true', 'aria-label': 'Sidebar' },
    el('span', { class: 'side-chevron', 'aria-hidden': 'true' }, '«'),
  );
  const side = el('div', { class: 'side' }, el('div', { class: 'side-head' }, toggle), body);
  const main = el('div', { class: 'side-main' });
  const ws = el('div', { class: 'ws' }, side, main);
  const anchor = anchorOf(page);
  const rest = [];
  for (let node = anchor ? anchor.nextSibling : page.firstChild; node; node = node.nextSibling) {
    if (!node.matches?.('[data-site-footer]')) rest.push(node);
  }
  if (anchor) anchor.after(ws);
  else page.prepend(ws);
  main.append(...rest);
  page.classList.add('has-side');

  const ownOpener = el('button', { type: 'button', class: 'plain side-open', 'aria-haspopup': 'dialog', 'aria-expanded': 'false' }, 'Menu');
  main.prepend(ownOpener);
  let opener = ownOpener;

  const title = el('h2', { class: 'side-drawer-title', id: `side-drawer-title-${n}` }, 'Menu');
  const close = el('button', { type: 'button', class: 'plain side-close', 'aria-label': 'Close' }, '×');
  const holder = el('div', { class: 'side-drawer-body' });
  const drawer = el('dialog', { class: 'side-drawer', 'aria-labelledby': title.id }, el('div', { class: 'side-drawer-head' }, title, close), holder);
  document.body.append(drawer);

  let links = null;
  let folders = null;
  let prefs = storedPrefs();
  let current = { side: 'none', label: null };
  const wide = globalThis.matchMedia?.(`(min-width: ${DRAWER_BELOW}px)`) ?? null;
  const width = () => (wide ? (wide.matches ? DRAWER_BELOW : DRAWER_BELOW - 1) : globalThis.innerWidth ?? DRAWER_BELOW);

  function layout() {
    current = layoutFor(width(), prefs, links ? 'vertical' : 'horizontal', { folders: Boolean(folders) });
    page.dataset.side = current.side;
    if (current.side !== 'drawer' && drawer.open) drawer.close();
    side.hidden = current.side === 'none' || current.side === 'drawer';
    body.hidden = current.side === 'rail';
    toggle.setAttribute('aria-expanded', String(current.side === 'column'));
    toggle.title = current.side === 'rail' ? 'Show the sidebar' : 'Hide the sidebar';
    toggle.firstChild.textContent = current.side === 'rail' ? '»' : '«';
    const label = current.label ?? 'Menu';
    title.textContent = label;
    close.setAttribute('aria-label', `Close ${label.toLowerCase()}`);
    for (const button of new Set([ownOpener, opener])) {
      const mine = button === opener;
      button.hidden = !mine || current.side !== 'drawer';
      if (mine && button.dataset.keepText !== 'true') button.textContent = label;
      button.setAttribute('aria-haspopup', 'dialog');
      button.setAttribute('aria-expanded', String(drawer.open));
    }
  }

  function openDrawer() {
    if (current.side !== 'drawer' || drawer.open) return;
    holder.append(body);
    body.hidden = false;
    drawer.showModal();
    opener.setAttribute('aria-expanded', 'true');
    const first =
      body.querySelector('[role=treeitem][tabindex="0"]') ?? body.querySelector('a[aria-current="page"]') ?? body.querySelector('a[href], button:not([hidden])');
    (first ?? close).focus();
  }

  drawer.addEventListener('close', () => {
    side.append(body);
    layout();
    if (!opener.hidden) opener.focus();
  });
  // A click on the backdrop (outside the drawer's box) closes it.
  drawer.addEventListener('click', (event) => {
    if (event.target === drawer) drawer.close();
  });
  close.addEventListener('click', () => drawer.close());
  // Following a link or opening a folder in the drawer closes it, so the page under it shows.
  holder.addEventListener('click', (event) => {
    if (event.target.closest?.('a[href]') && !event.defaultPrevented) drawer.close();
  });
  toggle.addEventListener('click', () => {
    prefs = { collapsed: current.side === 'column' };
    try {
      localStorage.setItem(SIDEBAR_KEY, prefsText(prefs));
    } catch {
      // not remembered
    }
    layout();
    toggle.focus();
  });
  const onOpener = () => (drawer.open ? drawer.close() : openDrawer());
  ownOpener.addEventListener('click', onOpener);
  if (wide) wide.addEventListener('change', layout);
  else globalThis.addEventListener?.('resize', layout);

  const controller = {
    side,
    body,
    main,
    drawer,
    /** The site's navigation at the top of the sidebar; null puts it back above the workspace. */
    links(view) {
      // The navigation drawn last may have replaced the one held here: what is in the sidebar goes back.
      if (!view) for (const old of body.querySelectorAll(':scope > [data-chrome]')) ws.before(old);
      links = view;
      if (view && view.parentNode !== body) body.prepend(view);
      layout();
    },
    /** The folder tree beneath the links; null takes it away. */
    folders(view) {
      if (folders && folders !== view) folders.remove();
      folders = view;
      if (view && view.parentNode !== body) body.append(view);
      layout();
    },
    /** The page's own button opens the drawer (its text stays when `keepText`). */
    opener(button, { keepText = false } = {}) {
      opener = button;
      button.dataset.keepText = String(keepText);
      button.removeAttribute('aria-controls');
      button.addEventListener('click', onOpener);
      ownOpener.remove();
      layout();
    },
    layout,
    get state() {
      return current;
    },
    /** Takes the workspace apart (nothing left to hold): the main column's content goes back. */
    dissolve() {
      if (drawer.open) drawer.close();
      ownOpener.remove();
      ws.replaceWith(...main.childNodes);
      drawer.remove();
      page.classList.remove('has-side');
      delete page.dataset.side;
      controllers.delete(page);
      if (wide) wide.removeEventListener?.('change', layout);
    },
  };
  controllers.set(page, controller);
  layout();
  return controller;
}
