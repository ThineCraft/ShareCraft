// One place for the keyboard shortcuts of a page. Each shortcut is registered with its keys
// ("Mod+K" is Ctrl+K, or Cmd+K on a Mac), what it does in plain words, where it belongs
// (everywhere, the library, the editor) and a condition; one keydown listener per page finds the
// shortcut a key press means and runs it. Keys typed in text fields, in menus and while a modal
// dialog is open are left alone (except Escape), unless a shortcut says it works there.
//
// "?" opens a Keyboard shortcuts sheet that lists the shortcuts this page has, made from the
// same registry. The matching (`parseKeys`, `comboMatches`, `findShortcut`) and the sheet's
// content (`keyParts`, `sheetGroups`) are pure and tested; the rest needs a page.
//
// This module imports nothing, so that every other module may use it.

/** Where a shortcut works, in the order the sheet lists them. */
export const SCOPES = Object.freeze([
  { id: 'global', heading: 'Everywhere' },
  { id: 'library', heading: 'In a library' },
  { id: 'editor', heading: 'In the editor' },
]);

/** Whether `platform` (navigator.platform) is a Mac, iPhone or iPad, where Cmd stands for Ctrl. */
export function isMacPlatform(platform = globalThis.navigator?.platform ?? '') {
  return /Mac|iPhone|iPad|iPod/.test(String(platform));
}

const NOT_TEXT = new Set(['checkbox', 'radio', 'button', 'submit', 'reset', 'range', 'color', 'file', 'image']);

/** Whether `target` (an element, or something shaped like one) takes typed text. */
export function isTextField(target) {
  if (!target) return false;
  if (target.isContentEditable) return true;
  const tag = String(target.tagName ?? '').toUpperCase();
  if (tag === 'TEXTAREA' || tag === 'SELECT') return true;
  if (tag === 'INPUT') return !NOT_TEXT.has(String(target.type ?? 'text').toLowerCase());
  return false;
}

const NAMES = {
  space: ' ',
  spacebar: ' ',
  esc: 'escape',
  up: 'arrowup',
  down: 'arrowdown',
  left: 'arrowleft',
  right: 'arrowright',
  del: 'delete',
};

/** A key as compared: one character in lower case, or a key name in lower case ("arrowup"). */
function normalKey(key) {
  const text = String(key ?? '');
  if (text.length === 1) return text.toLowerCase();
  const lower = text.toLowerCase();
  return NAMES[lower] ?? lower;
}

/**
 * The parts of a key text such as "Mod+Shift+F", "?", "Alt+ArrowUp" or "Space": `{ key, mod,
 * shift, alt }`. `Mod` (also written Ctrl, Cmd or Meta) is Ctrl, or Cmd on a Mac.
 */
export function parseKeys(text) {
  const parts = String(text ?? '').split('+');
  // "Mod++" would be Mod and "+": an empty last part means the key is "+" itself.
  let key = parts.pop();
  if (key === '' && parts.length && parts[parts.length - 1] === '') {
    parts.pop();
    key = '+';
  }
  const combo = { key: normalKey(key), mod: false, shift: false, alt: false };
  for (const part of parts) {
    const name = part.trim().toLowerCase();
    if (['mod', 'ctrl', 'control', 'cmd', 'command', 'meta'].includes(name)) combo.mod = true;
    else if (name === 'shift') combo.shift = true;
    else if (name === 'alt' || name === 'option') combo.alt = true;
  }
  return combo;
}

/** A printable key that is not a letter or digit ("?", "/"): Shift may be needed to type it. */
const isSymbol = (key) => key.length === 1 && !/[\p{L}\p{N}]/u.test(key);

/**
 * Whether a key press (`{ key, code?, ctrlKey, metaKey, shiftKey, altKey }`) is `combo` (from
 * `parseKeys`). Mod is Ctrl or Cmd, on any platform, as the shortcuts have always taken both.
 * A letter also matches by its physical key (`code` "KeyF") while Ctrl, Cmd or Alt is held, where
 * some layouts give another character. Shift must match, except for symbols such as "?" and "/",
 * which some keyboards type with Shift and some without.
 */
export function comboMatches(combo, event) {
  if (!combo || !event) return false;
  const key = normalKey(event.key);
  const held = Boolean(event.ctrlKey || event.metaKey || event.altKey);
  const byCode = /^[a-z]$/.test(combo.key) && held && event.code === `Key${combo.key.toUpperCase()}`;
  if (key !== combo.key && !byCode) return false;
  if (Boolean(event.ctrlKey || event.metaKey) !== combo.mod) return false;
  if (Boolean(event.altKey) !== combo.alt) return false;
  if (!isSymbol(combo.key) && Boolean(event.shiftKey) !== combo.shift) return false;
  return true;
}

/**
 * Where a key press happened, for `findShortcut`: in a text field (`field`), in an open menu
 * (`menu`), and whether a modal dialog is open (`dialog`). `doc` is the page's document.
 */
export function contextOf(event, doc = globalThis.document) {
  const target = event?.target ?? null;
  return {
    field: isTextField(target),
    menu: Boolean(target?.closest?.('[role=menu],[role=menubar],[role=listbox]')),
    dialog: Boolean(doc?.querySelector?.('dialog[open]')),
  };
}

/**
 * The first shortcut of `entries` that a key press means, or null. A shortcut is `{ keys,
 * label, scope, when?, inFields?, inMenus?, inDialogs? }`. A press already handled
 * (defaultPrevented) or in the middle of composing text means none. In a text field only
 * shortcuts with `inFields` count, in a menu only those with `inMenus`, and while a modal dialog
 * is open only those with `inDialogs` and Escape. Then `when(event, context)` must agree.
 */
export function findShortcut(entries, event, context = {}) {
  if (!event || event.defaultPrevented || event.isComposing) return null;
  const escape = normalKey(event.key) === 'escape';
  for (const entry of entries) {
    if (!entry.keys.some((keys) => comboMatches(parseKeys(keys), event))) continue;
    if (context.field && !entry.inFields) continue;
    if (context.menu && !entry.inMenus) continue;
    if (context.dialog && !entry.inDialogs && !escape) continue;
    if (entry.when && !entry.when(event, context)) continue;
    return entry;
  }
  return null;
}

const MAC_SIGNS = { mod: ['⌘', 'Command'], shift: ['⇧', 'Shift'], alt: ['⌥', 'Option'] };
const KEY_WORDS = {
  ' ': 'Space',
  arrowup: '↑',
  arrowdown: '↓',
  arrowleft: '←',
  arrowright: '→',
  escape: 'Esc',
  enter: 'Enter',
  delete: 'Delete',
  backspace: 'Backspace',
  home: 'Home',
  end: 'End',
  pageup: 'Page Up',
  pagedown: 'Page Down',
  tab: 'Tab',
};
const SPOKEN = { arrowup: 'Up arrow', arrowdown: 'Down arrow', arrowleft: 'Left arrow', arrowright: 'Right arrow', escape: 'Escape', '?': 'Question mark', '/': 'Slash' };

/**
 * The keys of a key text as shown, one per `<kbd>`: `[{ text, spoken }]`, `spoken` being what a
 * screen reader should say when `text` is a sign. On a Mac Mod is ⌘, Shift ⇧ and Alt ⌥;
 * elsewhere Ctrl, Shift and Alt.
 */
export function keyParts(text, { mac = false } = {}) {
  const combo = parseKeys(text);
  const parts = [];
  const sign = (name, word) => parts.push(mac ? { text: MAC_SIGNS[name][0], spoken: MAC_SIGNS[name][1] } : { text: word, spoken: word });
  if (combo.mod) sign('mod', 'Ctrl');
  if (combo.alt) sign('alt', 'Alt');
  if (combo.shift) sign('shift', 'Shift');
  const key = combo.key;
  const shown = KEY_WORDS[key] ?? (/^f\d{1,2}$/.test(key) ? key.toUpperCase() : key.length === 1 ? key.toUpperCase() : key);
  parts.push({ text: shown, spoken: SPOKEN[key] ?? shown });
  return parts;
}

const ARIA_NAMES = {
  ' ': 'Space',
  arrowup: 'ArrowUp',
  arrowdown: 'ArrowDown',
  arrowleft: 'ArrowLeft',
  arrowright: 'ArrowRight',
  escape: 'Escape',
  enter: 'Enter',
  delete: 'Delete',
  backspace: 'Backspace',
  home: 'Home',
  end: 'End',
  pageup: 'PageUp',
  pagedown: 'PageDown',
};

/** The `aria-keyshortcuts` value of key texts: "Control+K /" (Meta on a Mac). */
export function ariaKeys(keys, { mac = false } = {}) {
  return keys
    .map((text) => {
      const combo = parseKeys(text);
      const names = [];
      if (combo.mod) names.push(mac ? 'Meta' : 'Control');
      if (combo.alt) names.push('Alt');
      if (combo.shift) names.push('Shift');
      const key = combo.key;
      names.push(ARIA_NAMES[key] ?? (key.length === 1 || /^f\d{1,2}$/.test(key) ? key.toUpperCase() : key));
      return names.join('+');
    })
    .join(' ');
}

/**
 * The sheet's content from the registered `entries`: one group per scope that has any (in the
 * order of SCOPES), each `{ scope, heading, rows: [{ label, keys: [[{ text, spoken }]] }] }`.
 * Shortcuts with `listed: false`, or whose `shown()` says no on this page now, are left out.
 * Shortcuts with the same label are one row with each of their keys.
 */
export function sheetGroups(entries, { mac = false } = {}) {
  const groups = [];
  for (const scope of SCOPES) {
    const rows = [];
    for (const entry of entries) {
      if (entry.scope !== scope.id || entry.listed === false) continue;
      if (entry.shown && !entry.shown()) continue;
      const keys = (entry.display ?? entry.keys).map((text) => keyParts(text, { mac }));
      const same = rows.find((row) => row.label === entry.label);
      if (same) same.keys.push(...keys);
      else rows.push({ label: entry.label, keys });
    }
    if (rows.length) groups.push({ scope: scope.id, heading: scope.heading, rows });
  }
  return groups;
}

/**
 * A registry of shortcuts: `add(entry)` (returns a function that removes it again), `entries()`,
 * `dispatch(event, doc?)` (runs the shortcut a key press means; its `run(event, context)` may
 * answer false for "not handled", otherwise the key's default is prevented) and `listen(target)`.
 */
export function createRegistry() {
  const list = [];
  const onKey = (event) => registry.dispatch(event);
  const onFrameKey = (event) => registry.dispatch(event, { only: (entry) => entry.frames === true });
  const registry = {
    add(entry) {
      const full = { scope: 'global', ...entry, keys: [].concat(entry.keys) };
      list.push(full);
      return () => {
        const at = list.indexOf(full);
        if (at >= 0) list.splice(at, 1);
      };
    },
    entries: () => [...list],
    dispatch(event, { doc = globalThis.document, only = null } = {}) {
      const context = contextOf(event, doc);
      const found = findShortcut(only ? list.filter(only) : list, event, context);
      if (!found) return false;
      if (found.run(event, context) === false) return false;
      event.preventDefault();
      if (found.stop) event.stopPropagation();
      return true;
    },
    /**
     * The page's one keydown listener on `target` (the document). With `frames`, `target` is an
     * editor's window inside the page: there only shortcuts marked `frames` run, listened to
     * first (capture), as editors drawn on a canvas take every key.
     */
    listen(target, { frames = false } = {}) {
      // The same function each time: adding it again (a frame that loaded a new page lost its
      // listeners, the document has not) is a no-op for the browser.
      target?.addEventListener('keydown', frames ? onFrameKey : onKey, frames);
    },
  };
  return registry;
}

/** The page's registry; it starts listening on the document with the first shortcut. */
export const shortcuts = createRegistry();

/** Registers a shortcut on this page (see `findShortcut` for its fields) and returns its remover. */
export function addShortcut(entry) {
  if (typeof document !== 'undefined') shortcuts.listen(document);
  return shortcuts.add(entry);
}

// ---- the Keyboard shortcuts sheet ----

let sheet = null;

function el(tag, attributes = {}, ...children) {
  const element = document.createElement(tag);
  for (const [name, value] of Object.entries(attributes)) {
    if (value === null || value === undefined || value === false) continue;
    element.setAttribute(name, value === true ? '' : String(value));
  }
  element.append(...children.flat().filter((child) => child !== null && child !== undefined));
  return element;
}

/** One key as a `<kbd>`; a sign (⌘, ↑) also says its name to screen readers. */
function kbd(part) {
  if (part.spoken === part.text) return el('kbd', {}, part.text);
  return el('kbd', { title: part.spoken }, el('span', { 'aria-hidden': 'true' }, part.text), el('span', { class: 'visually-hidden' }, part.spoken));
}

/** Opens the Keyboard shortcuts sheet: the shortcuts of this page, grouped by where they work. */
export function openShortcutSheet() {
  if (typeof document === 'undefined') return;
  const mac = isMacPlatform();
  if (!sheet) {
    const close = el('button', { type: 'button' }, 'Close');
    sheet = el(
      'dialog',
      { class: 'shortcut-sheet', 'aria-labelledby': 'shortcut-title' },
      el('h2', { id: 'shortcut-title' }, 'Keyboard shortcuts'),
      el('p', { class: 'meta' }, 'The shortcuts of this page. They wait while you type in a field.'),
      el('div', { class: 'shortcut-groups' }),
      el('div', { class: 'row end' }, close),
    );
    close.addEventListener('click', () => sheet.close());
    sheet.addEventListener('click', (event) => {
      if (event.target === sheet) sheet.close();
    });
    let back = null;
    sheet.addEventListener('close', () => {
      if (back?.isConnected) back.focus();
      back = null;
    });
    sheet.remember = (element) => {
      back = element;
    };
    document.body.append(sheet);
  }
  if (sheet.open) return;
  const groups = sheetGroups(shortcuts.entries(), { mac });
  sheet.querySelector('.shortcut-groups').replaceChildren(
    ...groups.map((group) =>
      el(
        'section',
        { 'aria-labelledby': `shortcuts-${group.scope}` },
        el('h3', { id: `shortcuts-${group.scope}` }, group.heading),
        el(
          'table',
          { class: 'shortcut-table' },
          el('thead', { class: 'visually-hidden' }, el('tr', {}, el('th', { scope: 'col' }, 'Keys'), el('th', { scope: 'col' }, 'What it does'))),
          el(
            'tbody',
            {},
            group.rows.map((row) =>
              el(
                'tr',
                {},
                el(
                  'td',
                  { class: 'shortcut-keys' },
                  row.keys.flatMap((parts, at) => [
                    at > 0 ? el('span', { class: 'meta' }, ' or ') : null,
                    el('span', { class: 'shortcut-combo' }, parts.map(kbd)),
                  ]),
                ),
                el('td', {}, row.label),
              ),
            ),
          ),
        ),
      ),
    ),
  );
  sheet.remember(document.activeElement);
  sheet.showModal();
  sheet.querySelector('button').focus();
}

let sheetReady = false;

/** Registers "?" for the sheet (once per page); the palette and the account menu open it too. */
export function setupShortcutSheet() {
  if (sheetReady) return;
  sheetReady = true;
  addShortcut({
    keys: ['?'],
    label: 'Show these keyboard shortcuts',
    scope: 'global',
    inDialogs: true,
    // Over another dialog it waits; with the sheet open, "?" closes it again.
    when: (event, context) => !context.dialog || Boolean(sheet?.open),
    run: () => {
      if (sheet?.open) sheet.close();
      else openShortcutSheet();
    },
  });
}
