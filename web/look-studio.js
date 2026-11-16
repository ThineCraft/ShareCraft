// The look studio's state, without any DOM (Change the look, round 19): which part of the look is
// shown (tabs on wide screens, an accordion below them), the keys of the tab list, the preview's
// Light / Dark / Phone switch, the theme gallery's cards and which parts of the draft differ from
// the saved look. site-look.js wires these to the page.

import { canonical, comparable, draftFromLook, packageFonts, previewPalettes } from './look.js';

/** The parts of the look, in the order of the tabs. */
export const STUDIO_PARTS = Object.freeze(['theme', 'fonts', 'header', 'navigation', 'footer', 'history']);

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

/** The fields of a draft each tab holds; together they are every field of a draft. */
const PART_FIELDS = Object.freeze({
  theme: ['from', 'theme'],
  fonts: ['fonts'],
  header: ['logo', 'thumbnail', 'icon', 'logo_alt', 'header'],
  navigation: ['navigation', 'menu'],
  footer: ['footer'],
});

/**
 * The parts of `draft` that differ from the saved look `savedLook` (null for the default look),
 * for the marks on the tabs: a subset of theme, fonts, header, navigation and footer. It compares
 * the draft as it would be sent, the way the session's `status().dirty` does, so a part is marked
 * exactly when the draft has a change there that Apply would send.
 */
export function changedParts(draft, savedLook) {
  const now = comparable(draft);
  const was = comparable(draftFromLook(savedLook ?? null));
  return Object.entries(PART_FIELDS)
    .filter(([, fields]) => fields.some((field) => canonical(now[field]) !== canonical(was[field])))
    .map(([part]) => part);
}

// ---- Fonts ----

/**
 * The font gallery's groups, in order and without empty ones: built-in and organisation
 * packages from `brandPackages` (`GET /api/brand`'s `font_packages`), then `sitePackages`, each
 * as `{ label, cards }`, a card `{from, name, fonts}` with `fonts` the specimen's (see
 * [`packageFonts`]).
 */
export function packageGallery(brandPackages = [], sitePackages = []) {
  const card = (pkg) => ({ from: String(pkg?.from ?? ''), name: String(pkg?.name ?? '').trim() || 'Untitled', fonts: packageFonts(pkg) });
  return [
    ['Built in', (brandPackages ?? []).filter((p) => p.builtin)],
    ['Organisation', (brandPackages ?? []).filter((p) => !p.builtin)],
    ['This site', sitePackages ?? []],
  ]
    .filter(([, packages]) => packages.length)
    .map(([label, packages]) => ({ label, cards: packages.map(card) }));
}

/** The card the font gallery marks for `draft`: its package's source, or System for none. */
export function chosenPackage(draft) {
  return draft?.fonts?.from ?? 'builtin:system';
}

/** The note shown while the look keeps a copy of a package that has changed at its source since. */
export const FONTS_SOURCE_CHANGED = 'This font package changed in the brand centre — choose it again to use the new version.';

/**
 * Which font card the gallery marks for `draft` and the note beside it, `{checked, note}`
 * (`checked` '' for none, `note` null for none). `settings` is the look settings
 * (`{look, fonts_source_changed}`) and `offered` the sources of the packages offered. A kept
 * copy (the stored one, unchanged) of a package that has changed at its source since is noted
 * and its card left unmarked, so choosing it takes the new version; a kept copy of a package
 * no longer offered is noted the same way.
 */
export function fontsChoice(draft, settings, offered = []) {
  const chosen = chosenPackage(draft);
  const fonts = draft?.fonts ?? null;
  const isOffered = chosen === 'builtin:system' || offered.includes(chosen);
  if (!isOffered && fonts) {
    return {
      checked: '',
      note: `This look keeps its own copy of the fonts ${fonts.name}, which are no longer offered here. Choose a package to change them.`,
    };
  }
  const stale = Boolean(fonts?.keep) && Boolean(settings?.fonts_source_changed) && settings?.look?.fonts?.from === fonts?.from;
  if (stale) return { checked: '', note: FONTS_SOURCE_CHANGED };
  return { checked: isOffered ? chosen : '', note: null };
}

// ---- The limits for site owners ----

/** The choices the brand policy can turn off, in the order the studio shows them. */
export const STUDIO_LIMITS = Object.freeze(['builtinThemes', 'customColours', 'siteThemes', 'siteImages', 'siteFonts']);

/** Each limit: the policy field, whether `true` restricts, and why the choice is off, in words. */
const LIMIT_RULES = Object.freeze({
  builtinThemes: ['hide_builtin_themes', true, 'Built-in themes are turned off for site owners by your organisation. Choose one of its themes.'],
  customColours: ['custom_colours', false, 'Custom colours are turned off for site owners by your organisation. Choose one of its themes.'],
  siteThemes: ['site_themes', false, 'Themes for one site are turned off for site owners by your organisation.'],
  siteImages: ['site_images', false, 'Pictures from the site’s own libraries are turned off for site owners by your organisation. Choose a brand-centre image.'],
  siteFonts: ['site_fonts', false, 'Font packages for one site are turned off for site owners by your organisation.'],
});

/**
 * What the brand policy `policy` (`GET /api/brand`'s `policy`) turns off for the person:
 * `limited` is true for a site owner (not an administrator or brand manager, who are never
 * limited). For each of [`STUDIO_LIMITS`] `{disabled, reason}`: a choice turned off always
 * comes with the sentence that says why; one allowed has none. A missing policy limits nothing.
 */
export function studioLimits(policy, limited) {
  const out = {};
  for (const key of STUDIO_LIMITS) {
    const [field, restricts, reason] = LIMIT_RULES[key];
    const value = policy?.[field];
    const off = Boolean(limited) && typeof value === 'boolean' && value === restricts;
    out[key] = off ? { disabled: true, reason } : { disabled: false, reason: '' };
  }
  return out;
}

/**
 * The limit a gallery choice `from` falls under (`kind` `'themes'` or `'fonts'`), or null when
 * nothing limits it: built-in themes, custom colours, a site's own themes or font packages.
 * Organisation themes and packages and the built-in packages are always allowed.
 */
export function choiceLimit(from, limits, kind = 'themes') {
  const value = String(from ?? '');
  let limit = null;
  if (kind === 'fonts') limit = value.startsWith('site:') ? limits?.siteFonts : null;
  else if (value === 'custom') limit = limits?.customColours;
  else if (value.startsWith('builtin:')) limit = limits?.builtinThemes;
  else if (value.startsWith('site:')) limit = limits?.siteThemes;
  return limit?.disabled ? limit : null;
}

// ---- Copying another site's look ----

/** The sites a look can be copied from: every site in `sites` (`GET /api/sites`) but `currentId`, by name. */
export function copyChoices(sites, currentId) {
  return (Array.isArray(sites) ? sites : [])
    .filter((site) => site && site.id !== currentId)
    .sort((a, b) => String(a.name ?? '').localeCompare(String(b.name ?? ''), undefined, { sensitivity: 'base' }));
}
