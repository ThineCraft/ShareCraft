// Change the look, for the people who manage a site: a look studio. On wide screens the controls
// sit at the left in tabs (Theme, Header, Navigation, Footer, History) with Apply and Cancel
// pinned under them, and the preview stays in view at the right with a Light / Dark / Phone
// switch; below 1100 pixels the tabs stack as an accordion and the preview opens in a full-screen
// sheet. The theme is chosen from a gallery of cards (built-in, organisation and site themes, then
// Custom colours with the live contrast check and "Use suggested"); the header layout and
// background and the navigation are chosen from small pictures; logo, thumbnail and header image
// are tiles. Every change goes to a draft; Apply saves it, Cancel drops it. History lists the
// earlier looks, each with Preview (load it as the draft) and Restore (apply it again). Fonts are
// chosen from a gallery of font packages with specimens (and the site's own packages, which can
// be made here when allowed); the Header has a tab icon slot and Navigation a menu style.
// "Copy from another site…" loads another site's look as the draft; Apply copies it. Choices
// the organisation's limits turn off for site owners are shown disabled, each saying why. The
// draft and what Apply sends live in look.js (lookSession), the studio's state in
// look-studio.js; the server checks everything again.

import {
  api,
  can,
  colourField,
  confirmDialog,
  contrastRows,
  draftImageHref,
  footerView,
  formatDate,
  h,
  headerView,
  messageFor,
  pageFor,
  queryId,
  requireMe,
  seg,
  setupTopbar,
  showCrumbs,
  showMessage,
  siteChrome,
  themeSwatches,
} from './common.js';
import { BLEED_DEFAULT, bleedOf, showBleed } from './bleed.js';
import { ROLES, roleChoices } from './brand-fonts.js';
import { pickImage } from './image-picker.js';
import { fontSpecimen } from './look-card-view.js';
import {
  CHECK_FIELD,
  FOOTER_GROUP_LIMIT,
  FOOTER_LINK_LIMIT,
  applyFonts,
  applyLook,
  deriveDark,
  draftImageUrl,
  hexOf,
  lookSession,
  packageFonts,
  describeLookChange,
  mayReplaceDraft,
  parseColour,
  previewPalettes,
  sameTheme,
} from './look.js';
import {
  STUDIO_PARTS,
  STUDIO_WIDE,
  changedParts,
  choiceLimit,
  chosenCard,
  copyChoices,
  fontsChoice,
  packageGallery,
  partFromHash,
  previewPanes,
  selectTab,
  studioLimits,
  studioState,
  studioView,
  studioWide,
  swatchModel,
  tabKey,
  themeGallery,
  toggleSection,
} from './look-studio.js';

const $ = (id) => document.getElementById(id);
const error = $('error');
const MISSING = 'This site does not exist, or you do not have access to it.';

const me = await requireMe();
setupTopbar(me, error);
const siteId = queryId(location.search);
if (!siteId) fail(MISSING);
if (!(await showCrumbs($('crumbs'), siteId))) fail(MISSING);
let info = (await siteChrome(siteId)).site;
if (!info) fail(MISSING);
$('title').textContent = `Change the look of ${info.name}`;
document.title = `${info.name}: Change the look — ShareCraft`;
if (!can(info.access, 'manage_site')) fail('Only the owners of this site can change its look.');

const readOnly = Boolean(info.archived);

/** Each bleed control: its setting, how its value is read, and how it is shown. */
const BLEED_CONTROLS = Object.freeze([
  ['bleed-distance', 'distance', Number, (v) => `${v} px`],
  ['bleed-blur', 'blur', Number, (v) => `${v} px`],
  ['bleed-transparency', 'transparency', Number, (v) => `${v} %`],
  ['bleed-tint-light', 'tint_light', String, null],
  ['bleed-tint-dark', 'tint_dark', String, null],
]);

/** The banner bleeds the preview shows now (bleed.js), stopped before it is drawn again. */
let previewBleeds = [];

const [settingsAnswer, brandAnswer, historyAnswer] = await Promise.all([
  api('GET', `/sites/${seg(siteId)}/look/settings`),
  api('GET', '/brand'),
  api('GET', `/sites/${seg(siteId)}/look/history`),
]);
if (!settingsAnswer.ok) fail(messageFor(settingsAnswer.error));
const brand = brandAnswer.ok ? brandAnswer.data : { themes: [], images: [], font_packages: [], fonts: [] };
// The organisation's limits apply to site owners only: administrators and brand managers can
// always choose anything (`can_manage` is true for them).
const limits = studioLimits(brand.policy, !brand.can_manage);
let siteThemes = settingsAnswer.data.themes ?? [];
let sitePackages = settingsAnswer.data.font_packages ?? [];
const allThemes = () => [...(brand.themes ?? []), ...siteThemes];
const session = lookSession({
  siteId,
  settings: settingsAnswer.data,
  themes: allThemes(),
  brandImages: brand.images ?? [],
  send: api,
  readOnly,
});
let history = historyAnswer.ok ? historyAnswer.data : [];

function fail(text) {
  showMessage(error, text);
  throw new Error(text);
}

/** Shows `text` under the Apply bar; `ok` for a confirmation rather than a refusal. */
function note(text, ok = false) {
  $('apply-msg').classList.toggle('ok', ok);
  showMessage($('apply-msg'), text);
}

$('archived-banner').hidden = !readOnly;
$('content').hidden = false;
// An archived site keeps the bar for its Preview button (narrow screens), not Apply and Cancel.
$('actions').classList.toggle('read-only', readOnly);
for (const id of ['apply', 'cancel', 'apply-note', 'copy-open']) $(id).hidden = readOnly;

// ---- the studio: tabs on wide screens, an accordion below them ----

const wideQuery = matchMedia(`(min-width: ${STUDIO_WIDE}px)`);
let studio = studioState(partFromHash(location.hash));
// The room the studio has: the page less a site's sidebar when it has one.
const isWide = () => studioWide(window.innerWidth, $('content').parentElement?.clientWidth ?? 0);
let wasWide = null;

function drawStudio() {
  const wide = isWide();
  wasWide = wide;
  const view = studioView(studio, wide);
  $('content').classList.toggle('wide', wide);
  $('studio-tabs').hidden = !wide;
  for (const part of STUDIO_PARTS) {
    const shown = view[part];
    const section = $(`part-${part}`);
    const tab = $(`tab-${part}`);
    section.hidden = shown.hidden;
    $(`body-${part}`).hidden = shown.bodyHidden;
    $(`toggle-${part}`).setAttribute('aria-expanded', String(shown.expanded));
    tab.setAttribute('aria-selected', String(shown.selected));
    tab.tabIndex = shown.selected ? 0 : -1;
    if (wide) {
      section.setAttribute('role', 'tabpanel');
      section.setAttribute('aria-labelledby', `tab-${part}`);
    } else {
      section.removeAttribute('role');
      section.setAttribute('aria-labelledby', `toggle-${part}`);
    }
  }
}

/** Keeps the part in the address, so a reload comes back to it. */
function rememberPart(part) {
  try {
    window.history.replaceState(null, '', `${location.pathname}${location.search}#${part}`);
  } catch {
    // an address that cannot be replaced only loses the remembered tab
  }
}

for (const part of STUDIO_PARTS) {
  $(`tab-${part}`).addEventListener('click', () => {
    studio = selectTab(studio, part);
    rememberPart(part);
    drawStudio();
  });
  $(`toggle-${part}`).addEventListener('click', () => {
    studio = toggleSection(studio, part);
    if (studio.open.includes(part)) rememberPart(part);
    drawStudio();
  });
}

// A link to another part (#footer) opens it.
addEventListener('hashchange', () => {
  const part = partFromHash(location.hash);
  if (!part) return;
  studio = selectTab(studio, part);
  drawStudio();
});

// Arrow keys, Home and End move along the tabs and show the tab they land on.
$('studio-tabs').addEventListener('keydown', (event) => {
  const tabs = [...$('studio-tabs').querySelectorAll('[role=tab]')];
  const at = tabs.indexOf(document.activeElement);
  const next = at < 0 ? null : tabKey(event.key, at, tabs.length);
  if (next === null) return;
  event.preventDefault();
  const part = tabs[next].dataset.part;
  studio = selectTab(studio, part);
  rememberPart(part);
  drawStudio();
  tabs[next].focus();
});

function layoutChanged() {
  if (isWide() === wasWide) return;
  if (isWide() && $('preview-sheet').open) $('preview-sheet').close();
  drawStudio();
}
wideQuery.addEventListener('change', layoutChanged);
// The sidebar folding to a rail or opening again changes the room without changing the window.
new ResizeObserver(layoutChanged).observe($('content').parentElement);

// ---- the preview's place: at the right on wide screens, in a full-screen sheet below them ----

function openPreviewSheet() {
  if (wasWide || $('preview-sheet').open) return;
  $('preview-sheet').append($('studio-preview'));
  $('preview-sheet').showModal();
  $('preview-close').focus();
}

$('preview-open').addEventListener('click', openPreviewSheet);
$('preview-close').addEventListener('click', () => $('preview-sheet').close());
$('preview-sheet').addEventListener('close', () => {
  $('content').append($('studio-preview'));
  if (!wasWide) $('preview-open').focus();
});

let previewMode = 'light';
for (const radio of document.querySelectorAll('input[name=preview-mode]')) {
  radio.addEventListener('change', () => {
    previewMode = radio.value;
    showPreviewMode();
  });
}

function showPreviewMode() {
  const panes = previewPanes(previewMode);
  $('preview-light').hidden = !panes.light;
  $('preview-dark').hidden = !panes.dark;
  $('preview-stage').classList.toggle('phone', panes.phone);
}

// ---- picture choices: radio cards with a small drawing ----

let choiceCount = 0;

/**
 * A radio card in the radio group `name`: a native radio (so arrows move between the cards), a
 * small drawing `art`, a title and a line of `hint`. `onPick(value)` runs when it is chosen.
 */
function radioCard(name, value, title, hint, art, onPick) {
  choiceCount += 1;
  const id = `${name}-${choiceCount}`;
  const input = h('input', { type: 'radio', name, value, id, class: 'pick-input', disabled: readOnly, 'aria-describedby': hint ? `${id}-hint` : null });
  input.addEventListener('change', () => {
    if (input.checked) onPick(value);
  });
  return h(
    'label',
    { class: 'pick-card', for: id },
    input,
    art,
    h('span', { class: 'pick-title' }, title),
    hint ? h('span', { class: 'pick-hint', id: `${id}-hint` }, hint) : null,
  );
}

/** Marks the card of `value` in the radio group inside `box`. */
function checkCard(box, value) {
  for (const input of box.querySelectorAll('input[type=radio]')) input.checked = input.value === value;
}

/** A small drawing of a page: a header band, a navigation and lines of content. */
function pageArt(attributes) {
  return h(
    'span',
    { class: 'page-art', 'aria-hidden': 'true', ...attributes },
    h('span', { class: 'pa-band' }, h('span', { class: 'pa-logo' }), h('span', { class: 'pa-title' })),
    h('span', { class: 'pa-nav' }, h('span'), h('span'), h('span')),
    h('span', { class: 'pa-body' }, h('span'), h('span'), h('span')),
  );
}

let customCard = null;
const allPackages = () => [...(brand.font_packages ?? []), ...sitePackages];
let fieldCount = 0;

// The footer groups folded shut, by their place; drawing again keeps them folded.
const foldedGroups = new Set();

const PREVIEW_ROWS = [
  ['Quarterly report.docx', 'Edited today'],
  ['Brand guide.pdf', 'Edited yesterday'],
  ['Team photo.jpg', 'Edited last week'],
];

const SLOT_NAMES = { logo: 'the logo', thumbnail: 'the thumbnail', icon: 'the tab icon', 'header.image': 'the header image', 'footer.logo': 'the footer logo' };

/** The parts of the studio a person edits; while another site's look is loaded they wait. */
const EDITED_PARTS = ['theme', 'fonts', 'header', 'navigation', 'footer'];

const colours = {
  primary: colourField('colour-primary', 'Primary (links, buttons, focus ring)', { onInput: (v) => colourChanged('primary', v) }),
  secondary: colourField('colour-secondary', 'Secondary (a strong header or footer)', { onInput: (v) => colourChanged('secondary', v) }),
  primary_dark: colourField('colour-primary-dark', 'Primary in dark mode', { optional: true, onInput: (v) => colourChanged('primary_dark', v) }),
  secondary_dark: colourField('colour-secondary-dark', 'Secondary in dark mode', { optional: true, onInput: (v) => colourChanged('secondary_dark', v) }),
};

$('colour-fields').replaceChildren(...Object.values(colours).map((field) => field.element));

const LAYOUTS = [
  ['minimal', 'Minimal', 'One slim row'],
  ['compact', 'Compact', 'A small logo and title'],
  ['standard', 'Standard', 'A larger logo and title'],
  ['extended', 'Extended', 'A tall band'],
];
const BACKGROUNDS = [
  ['none', 'None', 'The page colour'],
  ['soft', 'Soft', 'A tint of the primary colour'],
  ['strong', 'Strong', 'The secondary colour'],
  ['image', 'Image', 'Extended layout only'],
];
const NAVIGATIONS = [
  ['horizontal', 'Across the top', 'Links in a row under the header'],
  ['vertical', 'In a column at the left', 'In the Menu on narrow screens'],
];

$('header-layout').replaceChildren(
  ...LAYOUTS.map(([value, title, hint]) =>
    radioCard('header-layout', value, title, hint, pageArt({ 'data-layout': value }), (v) => {
      session.change((draft) => {
        draft.header.layout = v;
        if (draft.header.layout !== 'extended' && draft.header.background === 'image') draft.header.background = 'none';
      });
      fillHeader();
      refresh();
    }),
  ),
);
$('header-background').replaceChildren(
  ...BACKGROUNDS.map(([value, title, hint]) =>
    radioCard('header-background', value, title, hint, pageArt({ 'data-background': value }), (v) => {
      session.change((draft) => (draft.header.background = v));
      refresh();
    }),
  ),
);
$('navigation-choice').replaceChildren(
  ...NAVIGATIONS.map(([value, title, hint]) =>
    radioCard('navigation', value, title, hint, pageArt({ 'data-navigation': value }), (v) => {
      session.change((draft) => (draft.navigation = v));
      refresh();
    }),
  ),
);
const MENUS = [
  ['cascading', 'Dropdown', 'A list under each top link'],
  ['mega', 'Mega menu', 'A wide panel of columns; the Menu on phones'],
];
$('menu-choice').replaceChildren(
  ...MENUS.map(([value, title, hint]) =>
    radioCard('menu-style', value, title, hint, pageArt({ 'data-menu': value }), (v) => {
      session.change((draft) => (draft.menu = v));
      refresh();
    }),
  ),
);

// ---- the theme ----

function colourChanged(field, value) {
  session.setColour(field, value);
  showDerived();
  refresh();
}

/** The worked-out dark colours as the placeholders of empty dark fields. */
function showDerived() {
  const theme = session.draft.theme;
  const derived = (light, role) => {
    const colour = parseColour(light ?? '');
    const dark = colour ? deriveDark(colour, role) : null;
    return dark ? hexOf(dark) : '';
  };
  if (!theme.primary_dark) colours.primary_dark.set('', derived(theme.primary, 'primary'));
  if (!theme.secondary_dark) colours.secondary_dark.set('', derived(theme.secondary, 'secondary'));
}

/** A small page in one mode: its secondary colour as the band, its primary as a button and a link. */
function miniPage(scheme, colour) {
  const mini = h(
    'span',
    { class: 'tc-mini', 'data-scheme': scheme, 'aria-hidden': 'true' },
    h('span', { class: 'tc-band' }),
    h('span', { class: 'tc-line' }),
    h('span', { class: 'tc-button' }),
  );
  mini.style.setProperty('--tc-primary', colour.primary);
  mini.style.setProperty('--tc-secondary', colour.secondary);
  return mini;
}

/** Puts the colours of `model` (a swatchModel) into a theme card's two small pages. */
function paintCard(card, model) {
  const [light, dark] = card.querySelectorAll('.tc-mini');
  for (const [mini, colour] of [
    [light, model.light],
    [dark, model.dark],
  ]) {
    mini.style.setProperty('--tc-primary', colour.primary);
    mini.style.setProperty('--tc-secondary', colour.secondary);
  }
  card.querySelector('.tc-colours').textContent =
    `Light mode: primary ${model.light.primary}, secondary ${model.light.secondary}. Dark mode: primary ${model.dark.primary}, secondary ${model.dark.secondary}.`;
}

/** One card of the gallery for `model`; `title` and `sub` default to the theme's name. */
function themeCard(model, title = model.name, sub = '') {
  choiceCount += 1;
  const id = `theme-card-${choiceCount}`;
  // A choice the organisation's limits turn off is shown disabled, saying why.
  const limit = choiceLimit(model.from, limits);
  const input = h('input', {
    type: 'radio',
    name: 'theme-card',
    value: model.from,
    id,
    class: 'pick-input',
    disabled: readOnly || Boolean(limit),
    'aria-describedby': limit ? `${id}-colours ${id}-why` : `${id}-colours`,
  });
  input.addEventListener('change', () => {
    if (input.checked) themeChosen(model.from);
  });
  const card = h(
    'label',
    { class: 'theme-card', for: id },
    input,
    h('span', { class: 'tc-pages' }, miniPage('light', model.light), miniPage('dark', model.dark)),
    h('span', { class: 'tc-name' }, title),
    sub ? h('span', { class: 'tc-sub' }, sub) : null,
    limit ? h('span', { class: 'choice-why', id: `${id}-why` }, limit.reason) : null,
    h('span', { class: 'tc-colours visually-hidden', id: `${id}-colours` }),
  );
  paintCard(card, model);
  return card;
}

function themeChosen(from) {
  const theme = allThemes().find((t) => t.from === from);
  if (theme) session.chooseTheme(theme);
  else session.change((draft) => (draft.from = 'custom'));
  fillTheme();
  refresh();
}

/** The gallery: a group per kind of theme, then Custom colours. Drawn again when the themes change. */
function fillThemeChoice() {
  const groups = themeGallery(brand.themes ?? [], siteThemes).map(({ label, cards }) =>
    h('div', { class: 'theme-group' }, h('h3', { class: 'theme-group-label' }, label), h('div', { class: 'theme-cards' }, ...cards.map((model) => themeCard(model)))),
  );
  customCard = themeCard({ ...swatchModel(session.draft.theme), from: 'custom' }, 'Custom colours', 'Your own primary and secondary');
  customCard.classList.add('custom');
  $('theme-gallery').replaceChildren(...groups, h('div', { class: 'theme-group' }, h('h3', { class: 'theme-group-label' }, 'Your own'), h('div', { class: 'theme-cards' }, customCard)));
}

$('theme-name').addEventListener('input', () => {
  session.setName($('theme-name').value);
  refresh();
});

function fillTheme() {
  const { theme } = session.draft;
  $('theme-name').value = theme.name;
  for (const [field, control] of Object.entries(colours)) control.set(theme[field] ?? '');
  showDerived();
}

/** The note about a theme changed or gone in the brand centre since the look copied it. */
function showSource() {
  const saved = session.settings?.look;
  const changed = Boolean(session.settings?.theme_source_changed) && saved && session.draft.from === saved.from;
  const source = changed ? allThemes().find((t) => t.from === saved.from) : null;
  const unchangedCopy = changed && sameTheme(session.draft.theme, saved.theme);
  $('source-note').hidden = !unchangedCopy;
  if (!unchangedCopy) return;
  $('source-text').textContent = source
    ? `The theme ${source.name} has changed since this look copied it. The site keeps its copy until you choose the theme again.`
    : `The theme this look copied (${saved.theme.name}) no longer exists. The site keeps its copy.`;
  $('source-update').hidden = !source || readOnly;
}

$('source-update').addEventListener('click', () => {
  const source = allThemes().find((t) => t.from === session.settings?.look?.from);
  if (!source) return;
  session.chooseTheme(source);
  fillTheme();
  refresh();
});

// Saving a theme for this site, and typing colours, follow the organisation's limits.
if (limits.siteThemes.disabled) {
  $('save-site-theme').disabled = true;
  $('save-site-theme').setAttribute('aria-describedby', 'site-theme-why');
  $('save-site-theme').after(h('span', { class: 'choice-why', id: 'site-theme-why' }, ` ${limits.siteThemes.reason}`));
}
if (limits.customColours.disabled) {
  $('custom-theme').prepend(h('p', { class: 'choice-why', id: 'custom-why' }, limits.customColours.reason));
  for (const control of [$('theme-name'), ...Object.values(colours).map((field) => field.input)]) {
    control.disabled = true;
    control.setAttribute('aria-describedby', 'custom-why');
  }
}

$('save-site-theme').addEventListener('click', async () => {
  note('');
  const { theme } = session.draft;
  const body = { name: theme.name.trim(), primary: theme.primary, secondary: theme.secondary };
  if (theme.primary_dark) body.primary_dark = theme.primary_dark;
  if (theme.secondary_dark) body.secondary_dark = theme.secondary_dark;
  const result = await api('POST', `/sites/${seg(siteId)}/themes`, body);
  if (!result.ok) return note(messageFor(result.error));
  siteThemes = [...siteThemes, result.data];
  session.setChoices({ themes: allThemes() });
  session.chooseTheme(result.data);
  fillThemeChoice();
  fillTheme();
  showSiteThemes();
  refresh();
  note(`Saved ${result.data.name} as a theme of this site. Apply to use it.`, true);
});

function showSiteThemes() {
  $('site-themes-heading').hidden = siteThemes.length === 0;
  $('site-themes').hidden = siteThemes.length === 0;
  $('site-themes').replaceChildren(
    ...siteThemes.map((theme) => {
      const remove = h('button', { type: 'button', class: 'secondary small', 'aria-label': `Delete the theme ${theme.name}`, disabled: readOnly }, 'Delete');
      remove.addEventListener('click', () => deleteSiteTheme(theme));
      return h('li', {}, themeSwatches(theme), h('span', { class: 'grow' }, theme.name), remove);
    }),
  );
}

async function deleteSiteTheme(theme) {
  note('');
  const sure = await confirmDialog(`Delete the theme ${theme.name}? A look that uses it keeps its own copy.`, 'Delete theme');
  if (!sure) return;
  const result = await api('DELETE', `/sites/${seg(siteId)}/themes/${seg(theme.id)}`);
  if (!result.ok) return note(messageFor(result.error));
  siteThemes = siteThemes.filter((t) => t.id !== theme.id);
  session.setChoices({ themes: allThemes() });
  fillThemeChoice();
  fillTheme();
  showSiteThemes();
  refresh();
  note(`Deleted the theme ${theme.name}.`, true);
}

// ---- fonts: the package gallery and the site's own packages ----

/** One card of the font gallery: the package's name and a specimen in its fonts. */
function packageCard(card) {
  choiceCount += 1;
  const id = `package-card-${choiceCount}`;
  const limit = choiceLimit(card.from, limits, 'fonts');
  const input = h('input', {
    type: 'radio',
    name: 'package-card',
    value: card.from,
    id,
    class: 'pick-input',
    disabled: readOnly || Boolean(limit),
    'aria-describedby': limit ? `${id}-why` : null,
  });
  input.addEventListener('change', () => {
    if (!input.checked) return;
    session.chooseFonts(allPackages().find((p) => p.from === card.from) ?? null);
    refresh();
  });
  return h(
    'label',
    { class: 'pick-card package-card', for: id },
    input,
    fontSpecimen(card.fonts, 'small'),
    h('span', { class: 'pick-title' }, card.name),
    limit ? h('span', { class: 'choice-why', id: `${id}-why` }, limit.reason) : null,
  );
}

function fillPackages() {
  const groups = packageGallery(brand.font_packages ?? [], sitePackages).map(({ label, cards }) =>
    h('div', { class: 'theme-group' }, h('h3', { class: 'theme-group-label' }, label), h('div', { class: 'package-cards' }, ...cards.map(packageCard))),
  );
  $('package-gallery').replaceChildren(...groups);
  $('site-packages-empty').hidden = sitePackages.length > 0;
  $('site-packages').replaceChildren(
    ...sitePackages.map((pkg) => {
      const remove = h('button', { type: 'button', class: 'secondary small', 'aria-label': `Delete the font package ${pkg.name}`, disabled: readOnly }, 'Delete');
      remove.addEventListener('click', () => deleteSitePackage(pkg));
      return h('li', {}, h('span', { class: 'grow' }, pkg.name), remove);
    }),
  );
  const off = readOnly || limits.siteFonts.disabled;
  $('new-site-package').disabled = off;
  $('site-package-why').textContent = limits.siteFonts.disabled ? limits.siteFonts.reason : '';
}

/**
 * The marked card, and a note when the look keeps a copy of fonts no longer offered, or of a
 * package changed at its source since (whose card stays unmarked until it is chosen again).
 */
function showFonts() {
  const offered = allPackages().map((p) => p.from);
  const { checked, note } = fontsChoice(session.draft, session.settings, offered);
  checkCard($('package-gallery'), checked);
  $('fonts-source-note').hidden = !note;
  $('fonts-source-note').textContent = note ?? '';
}

async function deleteSitePackage(pkg) {
  note('');
  const sure = await confirmDialog(`Delete the font package ${pkg.name}? A look that uses it keeps its own copy.`, 'Delete package');
  if (!sure) return;
  const result = await api('DELETE', `/sites/${seg(siteId)}/font-packages/${seg(pkg.id)}`);
  if (!result.ok) return note(messageFor(result.error));
  sitePackages = sitePackages.filter((p) => p.id !== pkg.id);
  fillPackages();
  refresh();
  note(`Deleted the font package ${pkg.name}.`, true);
}

// A package for this site: each font from the brand centre or a font file of a library, whose
// licence the person confirms.
const siteFonts = { display: null, content: null }; // {brand_font, name, family, url} | {doc, name, confirmed}
const spRoles = {};

function drawSiteFontChoice(which, label) {
  const chosen = siteFonts[which];
  const select = h('select', { id: `sp-${which}` });
  select.replaceChildren(
    h('option', { value: '' }, 'None'),
    ...(brand.fonts ?? []).map((font) => h('option', { value: font.id }, font.name)),
    h('option', { value: 'file' }, chosen?.doc ? `File: ${chosen.name}` : 'A font file from a library…'),
  );
  select.value = chosen?.brand_font ?? (chosen?.doc ? 'file' : '');
  select.addEventListener('change', async () => {
    if (select.value === 'file') {
      const picked = await pickImage({ siteId, title: `Choose the ${which} font file`, mode: 'font' });
      siteFonts[which] = picked?.doc ? { doc: picked.doc, name: picked.name, confirmed: false } : siteFonts[which];
    } else if (select.value) {
      const font = (brand.fonts ?? []).find((f) => f.id === select.value);
      siteFonts[which] = font ? { brand_font: font.id, name: font.name, family: font.family, url: font.url } : null;
    } else {
      siteFonts[which] = null;
    }
    drawSitePackageForm();
  });
  const parts = [h('label', { for: `sp-${which}` }, label), select];
  if (chosen?.doc) {
    const box = h('input', { type: 'checkbox', id: `sp-${which}-licence` });
    box.checked = chosen.confirmed;
    box.addEventListener('change', () => {
      chosen.confirmed = box.checked;
      checkSitePackage();
    });
    parts.push(h('label', { class: 'check', for: `sp-${which}-licence` }, box, ' Our organisation holds the licence for this font.'));
  }
  return h('div', { class: 'field' }, ...parts);
}

function drawSitePackageForm() {
  $('sp-fonts').replaceChildren(drawSiteFontChoice('display', 'Display font'), drawSiteFontChoice('content', 'Content font'));
  const has = { display: Boolean(siteFonts.display), content: Boolean(siteFonts.content) };
  $('sp-roles').replaceChildren(
    ...ROLES.map(([role, label]) => {
      const select = h('select', { id: `sp-role-${role}` });
      const choices = roleChoices(has);
      select.replaceChildren(
        ...choices.map((c) => h('option', { value: c.value, disabled: c.disabled }, c.disabled ? `${c.label} (${c.reason.replace(/\.$/, '').toLowerCase()})` : c.label)),
      );
      const before = spRoles[role];
      select.value = choices.some((c) => c.value === before && !c.disabled) ? before : 'system';
      spRoles[role] = select.value;
      select.addEventListener('change', () => {
        spRoles[role] = select.value;
        checkSitePackage();
      });
      return h('div', { class: 'field' }, h('label', { for: `sp-role-${role}` }, label), select);
    }),
  );
  checkSitePackage();
}

/** The live specimen (brand-centre fonts only: a file is shown once saved) and why Save is off. */
function checkSitePackage() {
  const listed = (font) => (font?.brand_font ? font : null);
  $('sp-specimen').replaceChildren(fontSpecimen(packageFonts({ display: listed(siteFonts.display), content: listed(siteFonts.content), roles: { ...spRoles } })));
  let why = '';
  if (!$('sp-name').value.trim()) why = 'Give the package a name.';
  else if (Object.values(siteFonts).some((f) => f?.doc && !f.confirmed)) why = 'Confirm that your organisation holds the licence for each font file.';
  $('sp-save').disabled = Boolean(why);
  $('sp-why').textContent = why;
}

$('sp-name').addEventListener('input', checkSitePackage);
$('new-site-package').addEventListener('click', () => {
  siteFonts.display = null;
  siteFonts.content = null;
  for (const [role] of ROLES) spRoles[role] = 'system';
  $('sp-name').value = '';
  $('site-package-form').hidden = false;
  drawSitePackageForm();
  $('sp-name').focus();
});
$('sp-cancel').addEventListener('click', () => ($('site-package-form').hidden = true));
$('site-package-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  checkSitePackage();
  if ($('sp-save').disabled) return;
  const fontBody = (font) => (!font ? null : font.brand_font ? { brand_font: font.brand_font } : { doc: font.doc, licence_confirmed: font.confirmed === true });
  const result = await api('POST', `/sites/${seg(siteId)}/font-packages`, {
    name: $('sp-name').value.trim(),
    display: fontBody(siteFonts.display),
    content: fontBody(siteFonts.content),
    roles: { ...spRoles },
  });
  if (!result.ok) return note(messageFor(result.error) + reasonOf(result) + problemsOf(result));
  sitePackages = [...sitePackages, result.data];
  $('site-package-form').hidden = true;
  session.chooseFonts(result.data);
  fillPackages();
  refresh();
  note(`Saved ${result.data.name} as a font package of this site. Apply to use it.`, true);
});

// ---- images: logo, thumbnail, header image, footer logo, tab icon ----

const SLOTS = {
  logo: {
    box: 'logo-slot',
    label: 'Logo',
    get: () => session.draft.logo,
    set: (image) => session.change((draft) => (draft.logo = image)),
  },
  thumbnail: {
    box: 'thumbnail-slot',
    label: 'Thumbnail (site cards and the editor bar)',
    get: () => session.draft.thumbnail,
    set: (image) => session.change((draft) => (draft.thumbnail = image)),
  },
  header: {
    box: 'header-image-slot',
    label: 'Header image (extended layout with an image background)',
    get: () => session.draft.header.image,
    set: (image) => session.change((draft) => (draft.header.image = image)),
  },
  footer: {
    box: 'footer-logo-slot',
    label: 'Footer logo',
    get: () => session.draft.footer.logo,
    set: (image) => session.change((draft) => (draft.footer.logo = image)),
  },
  icon: {
    box: 'icon-slot',
    label: 'Tab icon (a square PNG or SVG; without one, the thumbnail)',
    get: () => session.draft.icon,
    set: (image) => session.change((draft) => (draft.icon = image)),
  },
};

/** An image slot as a tile: the picture, what it is for, the image's name, Change and Remove. */
function drawSlot(slot) {
  const image = slot.get();
  const url = draftImageHref(draftImageUrl(image));
  const picture = h('span', { class: 'slot-picture', 'aria-hidden': 'true' });
  if (url) {
    const img = h('img', { src: url, alt: '' });
    img.addEventListener('error', () => (img.hidden = true));
    picture.append(img);
  } else {
    picture.append(h('span', { class: 'slot-empty' }, 'No picture'));
  }
  const [what, why] = slot.label.split(' (');
  const name = what.toLowerCase();
  const choose = h('button', { type: 'button', class: 'secondary small', disabled: readOnly }, image ? `Change the ${name}` : `Choose a ${name}`);
  choose.addEventListener('click', async () => {
    const picked = await pickImage({
      siteId,
      brandImages: brand.images ?? [],
      title: `Choose the ${name}`,
      brandOnly: limits.siteImages.disabled ? limits.siteImages.reason : null,
    });
    if (!picked) return;
    slot.set(picked);
    if (slot === SLOTS.header) session.change((draft) => (draft.header.background = 'image'));
    fillHeader();
    drawSlot(slot);
    refresh();
  });
  const remove = image ? h('button', { type: 'button', class: 'ghost small', disabled: readOnly, 'aria-label': `Remove the ${name}` }, 'Remove') : null;
  remove?.addEventListener('click', () => {
    slot.set(null);
    drawSlot(slot);
    refresh();
  });
  $(slot.box).replaceChildren(
    h(
      'div',
      { class: 'slot card' },
      picture,
      h('span', { class: 'slot-label' }, what),
      why ? h('span', { class: 'slot-why' }, why[0].toUpperCase() + why.slice(1).replace(/\)$/, '')) : null,
      h('span', { class: 'slot-name' }, image?.name ?? 'None'),
      h('span', { class: 'slot-actions' }, choose, remove),
    ),
  );
}

// ---- header and navigation ----

function fillHeader() {
  const { draft } = session;
  $('logo-alt').value = draft.logo_alt;
  checkCard($('header-layout'), draft.header.layout);
  checkCard($('header-background'), draft.header.background);
  $('header-align').value = draft.header.align;
  $('hide-title').checked = draft.header.hide_title;
  checkCard($('navigation-choice'), draft.navigation);
  checkCard($('menu-choice'), draft.menu);
  fillBleed();
}

$('logo-alt').addEventListener('input', () => {
  session.change((draft) => (draft.logo_alt = $('logo-alt').value));
  refresh();
});
$('header-align').addEventListener('change', () => {
  session.change((draft) => (draft.header.align = $('header-align').value));
  refresh();
});
$('hide-title').addEventListener('change', () => {
  session.change((draft) => (draft.header.hide_title = $('hide-title').checked));
  refresh();
});

// ---- the banner bleed (bleed.js) ----

function fillBleed() {
  const bleed = bleedOf(session.draft.header.bleed);
  $('bleed-enabled').checked = bleed.enabled;
  for (const [id, key, , show] of BLEED_CONTROLS) {
    $(id).value = String(bleed[key]);
    if (show) $(`${id}-out`).textContent = show(bleed[key]);
  }
}

$('bleed-enabled').addEventListener('change', () => {
  session.change((draft) => (draft.header.bleed = { ...bleedOf(draft.header.bleed), enabled: $('bleed-enabled').checked }));
  refresh();
});
for (const [id, key, read, show] of BLEED_CONTROLS) {
  $(id).addEventListener('input', () => {
    const value = read($(id).value);
    if (show) $(`${id}-out`).textContent = show(value);
    session.change((draft) => (draft.header.bleed = bleedOf({ ...bleedOf(draft.header.bleed), [key]: value })));
    refresh();
  });
}
$('bleed-reset').addEventListener('click', () => {
  session.change((draft) => (draft.header.bleed = { ...BLEED_DEFAULT }));
  fillBleed();
  refresh();
});

// ---- footer ----

function fillFooter() {
  const { footer } = session.draft;
  $('footer-enabled').checked = footer.enabled;
  $('footer-layout').value = footer.layout;
  $('footer-background').value = footer.background;
  $('footer-name').value = footer.name;
  drawGroups();
}

$('footer-enabled').addEventListener('change', () => {
  session.change((draft) => (draft.footer.enabled = $('footer-enabled').checked));
  refresh();
});
$('footer-layout').addEventListener('change', () => {
  session.change((draft) => (draft.footer.layout = $('footer-layout').value));
  drawGroups();
  refresh();
});
$('footer-background').addEventListener('change', () => {
  session.change((draft) => (draft.footer.background = $('footer-background').value));
  refresh();
});
$('footer-name').addEventListener('input', () => {
  session.change((draft) => (draft.footer.name = $('footer-name').value));
  refresh();
});

/** A labelled text box whose input runs `onInput`. */
function textField(label, value, maxlength, onInput) {
  fieldCount += 1;
  const id = `footer-field-${fieldCount}`;
  const input = h('input', { type: 'text', id, value, maxlength: String(maxlength), autocomplete: 'off', disabled: readOnly });
  input.value = value;
  input.addEventListener('input', () => {
    onInput(input.value);
    refresh();
  });
  return h('div', { class: 'field' }, h('label', { for: id }, label), input);
}

/** The footer's groups of links as cards that fold, as many as its layout allows; the draft keeps the rest. */
function drawGroups() {
  const { footer } = session.draft;
  const extended = footer.layout === 'extended';
  const limit = FOOTER_GROUP_LIMIT[extended ? 'extended' : 'simple'];
  // A footer with no group yet shows one empty group to add links to; it is not sent while empty.
  if (footer.groups.length === 0) footer.groups.push({ heading: '', links: [] });
  const groups = session.draft.footer.groups.slice(0, limit);
  const boxes = groups.map((group, at) => {
    const title = () => {
      const count = `${group.links.length} of ${FOOTER_LINK_LIMIT} links`;
      if (!extended) return `Links, ${count}`;
      return `Group ${at + 1}${group.heading.trim() ? `: ${group.heading.trim()}` : ''}, ${count}`;
    };
    const summary = h('summary', {}, title());
    const links = group.links.map((link, i) => {
      const remove = h('button', { type: 'button', class: 'ghost small', disabled: readOnly, 'aria-label': `Remove link ${i + 1}` }, 'Remove');
      remove.addEventListener('click', () => {
        group.links.splice(i, 1);
        drawGroups();
        refresh();
      });
      return h(
        'li',
        {},
        textField(`Link ${i + 1} label`, link.label, 60, (v) => (link.label = v)),
        textField(`Link ${i + 1} address`, link.url, 2000, (v) => (link.url = v)),
        remove,
      );
    });
    const add = h('button', { type: 'button', class: 'secondary small', disabled: readOnly || group.links.length >= FOOTER_LINK_LIMIT }, 'Add a link');
    add.addEventListener('click', () => {
      group.links.push({ label: '', url: '' });
      drawGroups();
      refresh();
    });
    const removeGroup =
      extended && groups.length > 1 ? h('button', { type: 'button', class: 'ghost small', disabled: readOnly }, `Remove group ${at + 1}`) : null;
    removeGroup?.addEventListener('click', () => {
      session.draft.footer.groups.splice(at, 1);
      foldedGroups.clear();
      drawGroups();
      refresh();
    });
    const card = h(
      'details',
      { class: 'footer-group card', open: !foldedGroups.has(at) },
      summary,
      h(
        'div',
        { class: 'footer-group-body' },
        extended
          ? textField(`Group ${at + 1} heading`, group.heading, 40, (v) => {
              group.heading = v;
              summary.textContent = title();
            })
          : h('p', { class: 'meta' }, 'Links (at most 8). Addresses start with https://, http:// or /.'),
        links.length ? h('ul', { class: 'list' }, ...links) : null,
        h('div', { class: 'toolbar' }, add, removeGroup),
      ),
    );
    card.addEventListener('toggle', () => {
      if (card.open) foldedGroups.delete(at);
      else foldedGroups.add(at);
    });
    return card;
  });
  const addGroup = extended ? h('button', { type: 'button', class: 'secondary small', disabled: readOnly || groups.length >= limit }, 'Add a group') : null;
  addGroup?.addEventListener('click', () => {
    session.change((draft) => draft.footer.groups.push({ heading: '', links: [] }));
    drawGroups();
    refresh();
  });
  $('footer-groups').replaceChildren(...boxes, addGroup);
}

// ---- the preview, the check and Apply ----

/** Draws the draft in each preview element, light and dark, with applyLook on that element only. */
function drawPreview() {
  const effective = session.effective(info.name);
  for (const bleed of previewBleeds) bleed.stop();
  previewBleeds = [];
  for (const [id, label] of [
    ['preview-light', 'Light mode'],
    ['preview-dark', 'Dark mode'],
  ]) {
    const panel = $(id);
    applyLook(panel, effective);
    applyFonts(panel, effective.fonts);
    const nav = h(
      'ul',
      { class: 'preview-nav' },
      h('li', {}, h('a', { href: '#', 'aria-current': 'page' }, 'Home')),
      h('li', {}, h('a', { href: '#' }, 'Documents')),
      h('li', {}, h('a', { href: '#' }, 'Images')),
    );
    const rows = h('ul', { class: 'list' }, ...PREVIEW_ROWS.map(([name, when]) => h('li', {}, h('a', { href: '#', class: 'grow' }, name), h('small', {}, when))));
    const buttons = h(
      'div',
      { class: 'preview-buttons' },
      h('button', { type: 'button', tabindex: '-1' }, 'Upload'),
      h('button', { type: 'button', class: 'secondary', tabindex: '-1' }, 'New folder'),
      h('a', { href: '#' }, 'A link'),
    );
    const header = headerView(info, effective, { draft: true });
    panel.replaceChildren(
      h('p', { class: 'preview-label' }, label),
      header,
      h('div', { class: effective.navigation === 'vertical' ? 'preview-body vertical' : 'preview-body' }, nav, h('div', {}, rows, buttons)),
      footerView(effective, { draft: true }) ?? h('p', { class: 'meta' }, 'No footer.'),
    );
    const bleed = showBleed(header, effective.header.bleed);
    if (bleed) previewBleeds.push(bleed);
  }
}

/** The draft's colours on the small drawings of the header and navigation choices. */
function paintChoices() {
  const { light } = previewPalettes(session.draft.theme);
  for (const id of ['part-header', 'part-navigation']) {
    $(id).style.setProperty('--studio-primary', light.accent);
    $(id).style.setProperty('--studio-secondary', light.header_bg);
  }
}

/** The gallery's marked card, the Custom card's colours, and whether the colour fields show. */
function showTheme(check) {
  const chosen = chosenCard(session.draft, allThemes());
  checkCard($('theme-gallery'), chosen);
  if (customCard) paintCard(customCard, { ...swatchModel(session.draft.theme), from: 'custom' });
  // The colour fields show for custom colours, and for any theme whose colours do not read.
  $('custom-theme').hidden = chosen !== 'custom' && check.ok;
}

/** The marks on the tabs (and accordion headings) of the parts with changes not applied yet. */
function showChangedParts() {
  const changed = changedParts(session.draft, session.settings?.look ?? null);
  for (const part of STUDIO_PARTS) {
    for (const mark of document.querySelectorAll(`#tab-${part} .tab-mark, #toggle-${part} .tab-mark`)) mark.hidden = !changed.includes(part);
  }
}

/** After every change: the preview, the contrast check, which fields apply, and Apply. */
function refresh() {
  drawPreview();
  paintChoices();
  const status = session.status();
  $('pairs').replaceChildren(
    ...contrastRows(status.check, (pair) => {
      session.useSuggested(pair);
      fillTheme();
      refresh();
      colours[CHECK_FIELD[pair.check]]?.input.focus();
    }),
  );
  showTheme(status.check);
  const { draft } = session;
  const extended = draft.header.layout === 'extended';
  const imageCard = $('header-background').querySelector('input[value=image]');
  imageCard.disabled = readOnly || !extended;
  $('align-field').hidden = !extended;
  $('header-image-slot').hidden = !(extended && draft.header.background === 'image');
  $('hide-title').disabled = readOnly || !draft.logo;
  // The bleed is the banner picture's, so it is set only for an image band.
  $('bleed-part').hidden = !(extended && draft.header.background === 'image');
  $('bleed-fields').hidden = !bleedOf(draft.header.bleed).enabled;
  for (const control of $('bleed-part').querySelectorAll('input, button')) control.disabled = readOnly;
  $('footer-fields').hidden = !draft.footer.enabled;
  $('menu-part').hidden = draft.navigation !== 'horizontal';
  $('menu-off').hidden = draft.navigation === 'horizontal';
  showFonts();
  showCopy();
  showSource();
  showChangedParts();
  $('apply').disabled = !status.canApply;
  $('cancel').disabled = readOnly || !status.dirty;
  let why = '';
  if (session.copy) why = `Apply copies the look of ${session.copy.name} to this site.`;
  else if (status.dirty && !status.check.ok) why = 'Fix the colours that do not read before applying.';
  else if (status.dirty && !draft.theme.name.trim()) why = 'Give the theme a name before applying.';
  else if (status.dirty) why = 'You have changes that are not applied yet.';
  else why = 'This is the site’s current look.';
  $('apply-note').textContent = why;
  $('actions').classList.toggle('dirty', status.dirty);
}

/** Puts the whole draft into the form and redraws. */
function fill() {
  fillThemeChoice();
  fillPackages();
  fillTheme();
  fillHeader();
  fillFooter();
  for (const slot of Object.values(SLOTS)) drawSlot(slot);
  refresh();
}

$('apply').addEventListener('click', async () => {
  note('');
  $('apply').disabled = true;
  const copying = session.copy?.name ?? null;
  const result = await session.apply();
  if (result.ok && copying) result.copied = copying;
  if (!result.ok) {
    if (result.error) note(messageFor(result.error) + reasonOf(result) + problemsOf(result));
    refresh();
    return;
  }
  const copied = result.copied;
  siteThemes = session.settings.themes ?? siteThemes;
  sitePackages = session.settings.font_packages ?? sitePackages;
  session.setChoices({ themes: allThemes() });
  fill();
  await afterChange(copied ? `The look of ${copied} is copied to this site.` : 'The new look is applied.');
});

$('cancel').addEventListener('click', () => {
  session.cancel();
  fill();
  note('The draft was dropped.', true);
});

/** The problems of a `not_allowed` refusal, in words. */
function problemsOf(result) {
  const problems = Array.isArray(result.data?.problems) ? result.data.problems.filter((p) => typeof p === 'string') : [];
  return problems.length ? `: ${problems.join('; ')}.` : '';
}

/** A refusal's reason, when the server gave one (`invalid_look`, `invalid_theme`). */
function reasonOf(result) {
  const reason = result.data?.reason;
  return typeof reason === 'string' && reason ? ` (${reason})` : '';
}

/** Draws the page's own look and the history again after the look changed. */
async function afterChange(text) {
  info = (await siteChrome(siteId)).site ?? info;
  await loadHistory();
  note(text, true);
}

// ---- history ----

async function loadHistory() {
  const result = await api('GET', `/sites/${seg(siteId)}/look/history`);
  if (result.ok) history = result.data;
  drawHistory();
}

function drawHistory() {
  $('history-empty').hidden = history.length > 0;
  $('history').replaceChildren(
    ...history.flatMap((entry, at) => {
      const when = formatDate(entry.at);
      const who = entry.by?.name ?? 'Someone no longer here';
      const summary = describeLookChange(entry.before, entry.look);
      const actions = (which, label) => {
        const preview = h('button', { type: 'button', class: 'secondary small', 'aria-label': `Preview the look ${label} of ${when}` }, 'Preview');
        preview.addEventListener('click', async () => {
          if (!(await mayReplaceDraft(session, () => confirmDialog('Previewing this look drops the changes you have not applied. Preview it?', 'Preview')))) return;
          session.preview(entry, which);
          fill();
          note(`The look ${label} of ${when} is now the draft. Apply it to use it.`, true);
          // On wide screens the preview is already in view; below them it opens in its sheet.
          openPreviewSheet();
        });
        const restore = readOnly ? null : h('button', { type: 'button', class: 'secondary small', 'aria-label': `Restore the look ${label} of ${when}` }, 'Restore');
        restore?.addEventListener('click', () => restoreEntry(entry, when, which, label));
        return [preview, restore];
      };
      const row = h(
        'li',
        {},
        h('span', { class: 'grow' }, `${when}, by ${who}`, h('br'), h('small', {}, at === 0 ? `${summary} (the current look)` : summary)),
        h('span', { class: 'actions' }, ...actions('after', 'after the change')),
      );
      // The oldest entry listed also offers the look before it, which no entry's own look is.
      if (at !== history.length - 1) return [row];
      const earlier = h(
        'li',
        {},
        h('span', { class: 'grow' }, 'Before that change', h('br'), h('small', {}, entry.before ? 'The look it replaced' : 'The plain default look the site had')),
        h('span', { class: 'actions' }, ...actions('before', 'before the change')),
      );
      return [row, earlier];
    }),
  );
}

async function restoreEntry(entry, when, which = 'after', label = 'after the change') {
  note('');
  if (!(await mayReplaceDraft(session, () => confirmDialog('Restoring drops the changes you have not applied. Restore?', 'Restore')))) return;
  const result = await session.restore(entry, which);
  if (result.ok) {
    siteThemes = session.settings.themes ?? siteThemes;
    sitePackages = session.settings.font_packages ?? sitePackages;
    fill();
    await afterChange(`The look ${label} of ${when} is applied again.`);
    return;
  }
  if (result.gone) {
    fill();
    const names = result.gone.map((slot) => SLOT_NAMES[slot] ?? slot).join(', ');
    note(`Some images of that look are no longer stored (${names}). It is loaded as a draft without them: choose new ones or apply it as it is.`);
    return;
  }
  note(messageFor(result.error) + reasonOf(result) + problemsOf(result));
}

// ---- copying the look of another site ----

/** The banner while a copy is loaded, and the parts made inert until Apply or Cancel. */
function showCopy() {
  const copy = session.copy;
  $('copy-banner').hidden = !copy;
  if (copy) {
    const extras = [copy.themes ? 'its own themes' : null, copy.fonts ? 'its own font packages' : null].filter(Boolean);
    $('copy-text').textContent =
      `The preview shows the look of ${copy.name}${extras.length ? `, with ${extras.join(' and ')}` : ''}. Apply copies it to this site; Cancel goes back. The other parts can be changed after that.`;
  }
  for (const part of EDITED_PARTS) $(`body-${part}`).inert = Boolean(copy);
}

let copySites = null; // GET /api/sites, once read
let copyChosen = null;

/** The site list of the dialog, filtered, as radio buttons. */
function drawCopySites() {
  const wanted = $('copy-filter').value.trim().toLocaleLowerCase();
  const shown = copyChoices(copySites ?? [], siteId).filter((site) => !wanted || String(site.name).toLocaleLowerCase().includes(wanted));
  $('copy-count').textContent = copySites === null ? 'Looking for sites…' : shown.length === 1 ? '1 site' : `${shown.length} sites`;
  $('copy-sites').replaceChildren(
    ...shown.map((site) => {
      const id = `copy-site-${site.id}`;
      const input = h('input', { type: 'radio', name: 'copy-site', id, value: site.id });
      input.checked = copyChosen?.id === site.id;
      input.addEventListener('change', () => {
        copyChosen = site;
        $('copy-load').disabled = false;
        $('copy-load-why').textContent = '';
      });
      return h('li', {}, h('label', { class: 'check', for: id }, input, ` ${site.name}`));
    }),
  );
}

$('copy-open').addEventListener('click', async () => {
  if (!(await mayReplaceDraft(session, () => confirmDialog('Copying another site’s look drops the changes you have not applied. Go on?', 'Go on')))) return;
  copyChosen = null;
  $('copy-filter').value = '';
  $('copy-load').disabled = true;
  $('copy-load-why').textContent = 'Choose a site first.';
  showMessage($('copy-msg'), '');
  for (const [box, limit] of [
    ['copy-themes', limits.siteThemes],
    ['copy-fonts', limits.siteFonts],
  ]) {
    $(box).checked = false;
    $(box).disabled = limit.disabled;
    $(`${box}-why`).textContent = limit.disabled ? limit.reason : '';
  }
  $('copy-dialog').showModal();
  $('copy-filter').focus();
  drawCopySites();
  if (copySites === null) {
    const result = await api('GET', '/sites');
    copySites = result.ok && Array.isArray(result.data) ? result.data : [];
    if (!result.ok) showMessage($('copy-msg'), messageFor(result.error));
    drawCopySites();
  }
});
$('copy-filter').addEventListener('input', drawCopySites);
$('copy-cancel').addEventListener('click', () => $('copy-dialog').close());
$('copy-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  if (!copyChosen) return;
  const source = copyChosen;
  const result = await api('GET', `/sites/${seg(source.id)}/look`);
  if (!result.ok) return showMessage($('copy-msg'), messageFor(result.error));
  session.loadCopy(source, result.data, { themes: $('copy-themes').checked, fonts: $('copy-fonts').checked });
  $('copy-dialog').close();
  refresh();
  note(`The look of ${source.name} is loaded as the draft. Apply to copy it.`, true);
  openPreviewSheet();
  $('copy-open').focus();
});

// Leaving with changes not applied asks first.
addEventListener('beforeunload', (event) => {
  if (session.status().dirty) event.preventDefault();
});

if (settingsAnswer.data.in_hub) {
  const hub = settingsAnswer.data.in_hub;
  $('hub-note').hidden = false;
  // A hub you cannot see is not named.
  const named = !hub.hidden && hub.id && hub.name;
  $('hub-note').replaceChildren(
    ...(named ? ['This site is in the hub ', h('a', { href: pageFor('site', hub.id) }, hub.name)] : ['This site is in a hub']),
    ', so its pages use the hub’s theme. The theme chosen here is kept and comes back if the site leaves the hub. The logo, header, navigation and footer are this site’s own.',
  );
}
if (!brandAnswer.ok) note(`The brand centre could not be read: ${messageFor(brandAnswer.error)}`);
drawStudio();
showPreviewMode();
showSiteThemes();
fill();
if (limits.customColours.disabled || limits.builtinThemes.disabled || limits.siteImages.disabled || limits.siteThemes.disabled || limits.siteFonts.disabled) {
  $('limits-note').hidden = false;
}
drawHistory();
