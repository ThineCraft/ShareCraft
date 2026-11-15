// The look studio's state, without any DOM (Change the look, round 19): which part of the look is
// shown (tabs on wide screens, an accordion below them), the keys of the tab list, the preview's
// Light / Dark / Phone switch, the theme gallery's cards and which parts of the draft differ from
// the saved look. site-look.js wires these to the page.

import { draftFromLook, previewPalettes, sameTheme } from './look.js';

/** The parts of the look, in the order of the tabs. */
export const STUDIO_PARTS = Object.freeze(['theme', 'header', 'navigation', 'footer', 'history']);

/** The window width in CSS pixels from which the controls and the preview sit side by side. */
export const STUDIO_WIDE = 1100;

/** The room the two panes need beside a site's sidebar: below it the studio stacks even on a wide window. */
export const STUDIO_ROOM = 840;

/** Whether the studio shows two panes: a window at least STUDIO_WIDE wide with STUDIO_ROOM for the studio. */
export function studioWide(windowWidth, room) {
  return windowWidth >= STUDIO_WIDE && room >= STUDIO_ROOM;
}

/** The tab a page address's hash names (`#header`), or null. */
export function partFromHash(hash) {
  const name = String(hash ?? '').replace(/^#/, '');
  return STUDIO_PARTS.includes(name) ? name : null;
}

/**
 * The state at the start: `{ tab, open }`, `tab` the part shown on wide screens and `open` the
 * parts expanded in the accordion (the tab's part only). `initial` is a part name or null.
 */
export function studioState(initial = null) {
  const tab = STUDIO_PARTS.includes(initial) ? initial : 'theme';
  return { tab, open: [tab] };
}

/** `state` with the tab `part` chosen; the accordion then opens on that part alone, so a narrower window shows it. */
export function selectTab(state, part) {
  if (!STUDIO_PARTS.includes(part)) return state;
  return { tab: part, open: [part] };
}

/**
 * `state` with the accordion section `part` opened or closed. Opening one makes it the tab, so
 * widening the window shows the part last opened; closing leaves the tab alone.
 */
export function toggleSection(state, part) {
  if (!STUDIO_PARTS.includes(part)) return state;
  if (state.open.includes(part)) return { ...state, open: state.open.filter((p) => p !== part) };
  return { tab: part, open: [...state.open, part] };
}

/**
 * How each part is drawn for `state` when `wide` (tabs) or not (accordion): per part
 * `{ hidden, bodyHidden, selected, expanded }`. Tabs show the chosen part only; the accordion
 * shows every part with the open ones' bodies.
 */
export function studioView(state, wide) {
  const view = {};
  for (const part of STUDIO_PARTS) {
    const open = state.open.includes(part);
    view[part] = wide
      ? { hidden: part !== state.tab, bodyHidden: false, selected: part === state.tab, expanded: true }
      : { hidden: false, bodyHidden: !open, selected: part === state.tab, expanded: open };
  }
  return view;
}

/**
 * Where a key moves focus in a tab list of `count` tabs from tab `index` (arrows wrap, Home and
 * End go to the ends): the new index, or null when the key is not the tab list's.
 */
export function tabKey(key, index, count) {
  if (count <= 0) return null;
  switch (key) {
    case 'ArrowRight':
    case 'ArrowDown':
      return (index + 1) % count;
    case 'ArrowLeft':
    case 'ArrowUp':
      return (index - 1 + count) % count;
    case 'Home':
      return 0;
    case 'End':
      return count - 1;
    default:
      return null;
  }
}

/** The preview's switch: what each choice shows. Phone is the light preview in a phone-wide frame. */
export const PREVIEW_MODES = Object.freeze(['light', 'dark', 'phone']);

/** `{ light, dark, phone }`: which preview panes show for `mode`, and whether the phone frame is on. */
export function previewPanes(mode) {
  const m = PREVIEW_MODES.includes(mode) ? mode : 'light';
  return { light: m !== 'dark', dark: m === 'dark', phone: m === 'phone' };
}

/**
 * What a theme card shows for `theme` (`{from, name, primary, ..., resolved?}`): its name and the
 * primary and secondary colours in light and dark mode as `#rrggbb` (the checked colours when
 * the server sent them, else worked out as the preview would draw them).
 */
export function swatchModel(theme) {
  const resolved = theme?.resolved?.light && theme?.resolved?.dark ? theme.resolved : previewPalettes(theme ?? {});
  return {
    from: theme?.from ?? 'custom',
    name: String(theme?.name ?? '').trim() || 'Untitled',
    light: { primary: resolved.light.accent, secondary: resolved.light.header_bg },
    dark: { primary: resolved.dark.accent, secondary: resolved.dark.header_bg },
  };
}

/**
 * The theme gallery's groups, in order and without empty ones: built-in and organisation themes
 * from `brandThemes`, then `siteThemes`, each as `{ label, cards }` of [`swatchModel`]s.
 */
export function themeGallery(brandThemes = [], siteThemes = []) {
  return [
    ['Built in', brandThemes.filter((t) => t.builtin)],
    ['Organisation', brandThemes.filter((t) => !t.builtin)],
    ['This site', siteThemes],
  ]
    .filter(([, themes]) => themes.length)
    .map(([label, themes]) => ({ label, cards: themes.map(swatchModel) }));
}

/** The card the gallery marks for `draft`: its theme's source when the page offers it, else 'custom'. */
export function chosenCard(draft, themes = []) {
  return draft?.from !== 'custom' && themes.some((t) => t.from === draft?.from) ? draft.from : 'custom';
}

const same = (a, b) => JSON.stringify(a ?? null) === JSON.stringify(b ?? null);

/** The links of a footer that would be sent: empty groups and links left out. */
function footerShape(footer) {
  const groups = (footer.groups ?? [])
    .map((g) => ({ heading: String(g.heading ?? ''), links: (g.links ?? []).filter((l) => l.label || l.url) }))
    .filter((g) => g.heading || g.links.length);
  return { ...footer, groups };
}

/**
 * The parts of `draft` that differ from the saved look `savedLook` (null for the default look),
 * for the marks on the tabs: a subset of theme, header, navigation and footer.
 */
export function changedParts(draft, savedLook) {
  const saved = draftFromLook(savedLook ?? null);
  const parts = [];
  if (draft.from !== saved.from || !sameTheme(draft.theme, saved.theme)) parts.push('theme');
  if (!same(draft.logo, saved.logo) || !same(draft.thumbnail, saved.thumbnail) || draft.logo_alt !== saved.logo_alt || !same(draft.header, saved.header)) {
    parts.push('header');
  }
  if (draft.navigation !== saved.navigation) parts.push('navigation');
  if (!same(footerShape(draft.footer), footerShape(saved.footer))) parts.push('footer');
  return parts;
}
