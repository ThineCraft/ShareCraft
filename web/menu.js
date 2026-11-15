// Disclosure menus: a button that opens a short list of actions. The button carries
// aria-haspopup and aria-expanded; the list is role="menu" with role="menuitem" entries (buttons
// or links). Arrow keys, Home and End move between the entries, a letter jumps to the next entry
// that starts with it, Escape closes and puts focus back on the button, Tab closes and moves on,
// and a click anywhere else closes. One menu is open at a time.
//
// The key handling is pure (`menuKey`, `buttonKey`, `typeahead`) so that it is tested without a
// page; `setupMenu` wires it to the elements.

/**
 * What a key does while focus is on entry `index` of an open menu of `count` entries:
 * `{ focus: n }` (move to entry n), `{ close: 'refocus' }` (close and focus the button: Escape),
 * `{ close: 'leave' }` (close and let focus move on: Tab) or null (the key is not the menu's).
 * Down and Up wrap around.
 */
export function menuKey(key, index, count) {
  if (key === 'Escape' || key === 'Esc') return { close: 'refocus' };
  if (key === 'Tab') return { close: 'leave' };
  if (count <= 0) return null;
  const at = index >= 0 && index < count ? index : -1;
  switch (key) {
    case 'ArrowDown':
    case 'Down':
      return { focus: at === -1 ? 0 : (at + 1) % count };
    case 'ArrowUp':
    case 'Up':
      return { focus: at === -1 ? count - 1 : (at - 1 + count) % count };
    case 'Home':
    case 'PageUp':
      return { focus: 0 };
    case 'End':
    case 'PageDown':
      return { focus: count - 1 };
    default:
      return null;
  }
}

/**
 * What a key does on the closed menu's button: 'first' (open on the first entry: Down), 'last'
 * (open on the last entry: Up) or null. Enter and Space are the button's own click, which opens
 * the menu on its first entry.
 */
export function buttonKey(key) {
  if (key === 'ArrowDown' || key === 'Down') return 'first';
  if (key === 'ArrowUp' || key === 'Up') return 'last';
  return null;
}

/**
 * The entry a typed character jumps to: the next label after `index` that starts with `char`
 * (ignoring case and leading space), wrapping around; -1 when none does or `char` is not a single
 * printable character.
 */
export function typeahead(labels, index, char) {
  if (typeof char !== 'string' || [...char].length !== 1 || char.trim() === '') return -1;
  const wanted = char.toLocaleLowerCase();
  const count = labels.length;
  for (let step = 1; step <= count; step += 1) {
    const at = (((index < 0 ? -1 : index) + step) % count + count) % count;
    if (String(labels[at] ?? '').trim().toLocaleLowerCase().startsWith(wanted)) return at;
  }
  return -1;
}

// ---- wiring (needs a page) ----

let openNow = null; // { button, menu, close } of the open menu
let listening = false;

function listenOnce() {
  if (listening) return;
  listening = true;
  // A press anywhere outside the open menu and its button closes it, leaving focus where it lands.
  document.addEventListener(
    'pointerdown',
    (event) => {
      if (!openNow) return;
      const target = event.target;
      if (openNow.menu.contains(target) || openNow.button.contains(target)) return;
      openNow.close(false);
    },
    true,
  );
}

// Entries: plain items, and radio or checkbox items (which keep the menu open when chosen).
const ITEM = '[role="menuitem"],[role="menuitemradio"],[role="menuitemcheckbox"]';

/** The entries of `menu` that are shown, in order. */
export function menuItems(menu) {
  return [...menu.querySelectorAll(ITEM)].filter(
    (item) => !item.hidden && !item.closest('[hidden]:not([role="menu"])') && !item.disabled,
  );
}

/**
 * Makes `button` open and close `menu` (an element holding `[role=menuitem]` entries). The
 * button gets aria-haspopup, aria-expanded and aria-controls (`menu` must have an id); entries
 * get tabindex -1. Choosing an entry closes the menu and puts focus back on the button before
 * the entry's own click handler runs, so a dialog it opens returns focus there. Returns
 * `{ open(where), close(refocus) }`.
 */
export function setupMenu(button, menu) {
  listenOnce();
  button.setAttribute('aria-haspopup', 'menu');
  button.setAttribute('aria-expanded', 'false');
  if (menu.id) button.setAttribute('aria-controls', menu.id);
  menu.setAttribute('role', 'menu');
  if (button.id && !menu.hasAttribute('aria-label')) menu.setAttribute('aria-labelledby', button.id);
  menu.hidden = true;

  const prepare = () => {
    for (const item of menu.querySelectorAll(ITEM)) item.tabIndex = -1;
  };

  const close = (refocus) => {
    if (menu.hidden) return;
    menu.hidden = true;
    button.setAttribute('aria-expanded', 'false');
    if (openNow?.menu === menu) openNow = null;
    if (refocus) button.focus();
  };

  const open = (where = 'first') => {
    if (openNow && openNow.menu !== menu) openNow.close(false);
    prepare();
    menu.hidden = false;
    button.setAttribute('aria-expanded', 'true');
    openNow = { button, menu, close };
    const items = menuItems(menu);
    const target = where === 'last' ? items[items.length - 1] : items[0];
    target?.focus();
  };

  button.addEventListener('click', () => {
    if (menu.hidden) open('first');
    else close(true);
  });
  button.addEventListener('keydown', (event) => {
    const where = buttonKey(event.key);
    if (!where) return;
    event.preventDefault();
    open(where);
  });
  menu.addEventListener('keydown', (event) => {
    const items = menuItems(menu);
    const index = items.indexOf(document.activeElement);
    const result = menuKey(event.key, index, items.length);
    if (result?.focus !== undefined) {
      event.preventDefault();
      items[result.focus].focus();
      return;
    }
    if (result?.close === 'refocus') {
      event.preventDefault();
      event.stopPropagation(); // not also the Escape of a dialog around the menu
      close(true);
      return;
    }
    if (result?.close === 'leave') {
      // Focus goes back to the button first, so Tab moves on from there, not from inside.
      close(true);
      return;
    }
    if (event.key === ' ' && document.activeElement?.tagName === 'A') {
      event.preventDefault();
      document.activeElement.click();
      return;
    }
    if (event.ctrlKey || event.metaKey || event.altKey) return;
    const jump = typeahead(
      items.map((item) => item.textContent),
      index,
      event.key,
    );
    if (jump >= 0) {
      event.preventDefault();
      items[jump].focus();
    }
  });
  // Choosing an entry: close first, focus back on the button, then the entry's own handler runs.
  menu.addEventListener(
    'click',
    (event) => {
      if (event.target.closest?.('[role="menuitem"]')) close(true);
    },
    true,
  );
  // Focus leaving both the menu and its button (a click elsewhere, a screen reader jump) closes it.
  menu.addEventListener('focusout', (event) => {
    const next = event.relatedTarget;
    if (next && (menu.contains(next) || button.contains(next))) return;
    if (next) close(false);
  });
  return { open, close };
}
