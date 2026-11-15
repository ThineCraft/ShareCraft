// Change the look, for the people who manage a site: its theme (built-in, organisation, site or
// custom colours, with the live contrast check and "Use suggested"), its logo, thumbnail and
// header, its navigation style and its footer. Every change goes to a draft shown in a preview
// in light and dark mode side by side; Apply saves it, Cancel drops it. History lists the earlier
// looks, each with Preview (load it as the draft) and Restore (apply it again). The draft and
// what Apply sends live in look.js (lookSession); the server checks everything again.

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
import { pickImage } from './image-picker.js';
import {
  CHECK_FIELD,
  FOOTER_GROUP_LIMIT,
  FOOTER_LINK_LIMIT,
  applyLook,
  deriveDark,
  draftImageUrl,
  hexOf,
  lookSession,
  mayReplaceDraft,
  parseColour,
  previewEffective,
  sameTheme,
} from './look.js';

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
const [settingsAnswer, brandAnswer, historyAnswer] = await Promise.all([
  api('GET', `/sites/${seg(siteId)}/look/settings`),
  api('GET', '/brand'),
  api('GET', `/sites/${seg(siteId)}/look/history`),
]);
if (!settingsAnswer.ok) fail(messageFor(settingsAnswer.error));
const brand = brandAnswer.ok ? brandAnswer.data : { themes: [], images: [] };
let siteThemes = settingsAnswer.data.themes ?? [];
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
$('actions').hidden = readOnly;
$('history-section').hidden = false;

// ---- the theme ----

const colours = {
  primary: colourField('colour-primary', 'Primary (links, buttons, focus ring)', { onInput: (v) => colourChanged('primary', v) }),
  secondary: colourField('colour-secondary', 'Secondary (a strong header or footer)', { onInput: (v) => colourChanged('secondary', v) }),
  primary_dark: colourField('colour-primary-dark', 'Primary in dark mode', { optional: true, onInput: (v) => colourChanged('primary_dark', v) }),
  secondary_dark: colourField('colour-secondary-dark', 'Secondary in dark mode', { optional: true, onInput: (v) => colourChanged('secondary_dark', v) }),
};
$('colour-fields').replaceChildren(...Object.values(colours).map((field) => field.element));

function colourChanged(field, value) {
  session.setColour(field, value);
  $('theme-choice').value = 'custom';
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

function fillThemeChoice() {
  const option = (theme) => h('option', { value: theme.from }, theme.name);
  const groups = [
    ['Built in', (brand.themes ?? []).filter((t) => t.builtin)],
    ['Organisation', (brand.themes ?? []).filter((t) => !t.builtin)],
    ['This site', siteThemes],
  ].filter(([, themes]) => themes.length);
  $('theme-choice').replaceChildren(
    ...groups.map(([label, themes]) => h('optgroup', { label }, ...themes.map(option))),
    h('option', { value: 'custom' }, 'Custom colours'),
  );
}

$('theme-choice').addEventListener('change', () => {
  const value = $('theme-choice').value;
  const theme = allThemes().find((t) => t.from === value);
  if (theme) session.chooseTheme(theme);
  else session.change((draft) => (draft.from = 'custom'));
  fillTheme();
  refresh();
});

$('theme-name').addEventListener('input', () => {
  session.setName($('theme-name').value);
  $('theme-choice').value = 'custom';
  refresh();
});

function fillTheme() {
  const { theme, from } = session.draft;
  $('theme-choice').value = allThemes().some((t) => t.from === from) ? from : 'custom';
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
      const remove = h('button', { type: 'button', class: 'plain', 'aria-label': `Delete the theme ${theme.name}`, disabled: readOnly }, 'Delete');
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

// ---- images: logo, thumbnail, header image, footer logo ----

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
};

function drawSlot(slot) {
  const image = slot.get();
  const url = draftImageHref(draftImageUrl(image));
  const picture = h('span', { class: 'slot-picture', 'aria-hidden': 'true' });
  if (url) {
    const img = h('img', { src: url, alt: '' });
    img.addEventListener('error', () => (img.hidden = true));
    picture.append(img);
  }
  const what = slot.label.split(' (')[0].toLowerCase();
  const choose = h('button', { type: 'button', class: 'plain', disabled: readOnly }, image ? `Change the ${what}` : `Choose a ${what}`);
  choose.addEventListener('click', async () => {
    const picked = await pickImage({ siteId, brandImages: brand.images ?? [], title: `Choose the ${what}` });
    if (!picked) return;
    slot.set(picked);
    if (slot === SLOTS.header) session.change((draft) => (draft.header.background = 'image'));
    fillHeader();
    drawSlot(slot);
    refresh();
  });
  const remove = image ? h('button', { type: 'button', class: 'plain', disabled: readOnly, 'aria-label': `Remove the ${what}` }, 'Remove') : null;
  remove?.addEventListener('click', () => {
    slot.set(null);
    drawSlot(slot);
    refresh();
  });
  $(slot.box).replaceChildren(
    h('div', { class: 'slot' }, h('span', { class: 'slot-label' }, slot.label), picture, h('span', { class: 'slot-name' }, image?.name ?? 'None'), choose, remove),
  );
}

// ---- header and navigation ----

function fillHeader() {
  const { draft } = session;
  $('logo-alt').value = draft.logo_alt;
  $('header-layout').value = draft.header.layout;
  $('header-background').value = draft.header.background;
  $('header-align').value = draft.header.align;
  $('hide-title').checked = draft.header.hide_title;
  $(draft.navigation === 'vertical' ? 'nav-vertical' : 'nav-horizontal').checked = true;
}

$('logo-alt').addEventListener('input', () => {
  session.change((draft) => (draft.logo_alt = $('logo-alt').value));
  refresh();
});
$('header-layout').addEventListener('change', () => {
  session.change((draft) => {
    draft.header.layout = $('header-layout').value;
    if (draft.header.layout !== 'extended' && draft.header.background === 'image') draft.header.background = 'none';
  });
  fillHeader();
  refresh();
});
$('header-background').addEventListener('change', () => {
  session.change((draft) => (draft.header.background = $('header-background').value));
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
for (const id of ['nav-horizontal', 'nav-vertical']) {
  $(id).addEventListener('change', () => {
    session.change((draft) => (draft.navigation = $(id).value));
    refresh();
  });
}

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

let fieldCount = 0;
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

/** The footer's groups of links, as many as its layout allows; the draft keeps the rest. */
function drawGroups() {
  const { footer } = session.draft;
  const extended = footer.layout === 'extended';
  const limit = FOOTER_GROUP_LIMIT[extended ? 'extended' : 'simple'];
  // A footer with no group yet shows one empty group to add links to; it is not sent while empty.
  if (footer.groups.length === 0) footer.groups.push({ heading: '', links: [] });
  const groups = session.draft.footer.groups.slice(0, limit);
  const boxes = groups.map((group, at) => {
    const links = group.links.map((link, i) => {
      const remove = h('button', { type: 'button', class: 'plain', disabled: readOnly, 'aria-label': `Remove link ${i + 1}` }, 'Remove');
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
    const add = h('button', { type: 'button', class: 'plain', disabled: readOnly || group.links.length >= FOOTER_LINK_LIMIT }, 'Add a link');
    add.addEventListener('click', () => {
      group.links.push({ label: '', url: '' });
      drawGroups();
      refresh();
    });
    const removeGroup =
      extended && groups.length > 1 ? h('button', { type: 'button', class: 'plain', disabled: readOnly }, `Remove group ${at + 1}`) : null;
    removeGroup?.addEventListener('click', () => {
      session.draft.footer.groups.splice(at, 1);
      drawGroups();
      refresh();
    });
    return h(
      'div',
      { class: 'footer-group', role: 'group', 'aria-label': extended ? `Group ${at + 1}` : 'Links' },
      extended ? textField(`Group ${at + 1} heading`, group.heading, 40, (v) => (group.heading = v)) : h('p', { class: 'meta' }, 'Links (at most 8). Addresses start with https://, http:// or /.'),
      links.length ? h('ul', { class: 'list' }, ...links) : null,
      h('div', { class: 'toolbar' }, add, removeGroup),
    );
  });
  const addGroup = extended ? h('button', { type: 'button', class: 'plain', disabled: readOnly || groups.length >= limit }, 'Add a group') : null;
  addGroup?.addEventListener('click', () => {
    session.change((draft) => draft.footer.groups.push({ heading: '', links: [] }));
    drawGroups();
    refresh();
  });
  $('footer-groups').replaceChildren(...boxes, addGroup);
}

// ---- the preview, the check and Apply ----

const PREVIEW_ROWS = [
  ['Quarterly report.docx', 'Edited today'],
  ['Brand guide.pdf', 'Edited yesterday'],
  ['Team photo.jpg', 'Edited last week'],
];

/** Draws the draft in each preview element, light and dark, with applyLook on that element only. */
function drawPreview() {
  const effective = previewEffective(session.draft, info.name);
  for (const [id, label] of [
    ['preview-light', 'Light mode'],
    ['preview-dark', 'Dark mode'],
  ]) {
    const panel = $(id);
    applyLook(panel, effective);
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
      h('button', { type: 'button', class: 'plain', tabindex: '-1' }, 'New folder'),
      h('a', { href: '#' }, 'A link'),
    );
    panel.replaceChildren(
      h('p', { class: 'preview-label' }, label),
      headerView(info, effective, { draft: true }),
      h('div', { class: effective.navigation === 'vertical' ? 'preview-body vertical' : 'preview-body' }, nav, h('div', {}, rows, buttons)),
      footerView(effective, { draft: true }) ?? h('p', { class: 'meta' }, 'No footer.'),
    );
  }
}

/** After every change: the preview, the contrast check, which fields apply, and Apply. */
function refresh() {
  drawPreview();
  const status = session.status();
  $('pairs').replaceChildren(
    ...contrastRows(status.check, (pair) => {
      session.useSuggested(pair);
      fillTheme();
      refresh();
      colours[CHECK_FIELD[pair.check]]?.input.focus();
    }),
  );
  const { draft } = session;
  const extended = draft.header.layout === 'extended';
  $('header-background').querySelector('option[value=image]').disabled = !extended;
  $('align-field').hidden = !extended;
  $('header-image-slot').hidden = !(extended && draft.header.background === 'image');
  $('hide-title').disabled = readOnly || !draft.logo;
  $('footer-fields').hidden = !draft.footer.enabled;
  showSource();
  $('apply').disabled = !status.canApply;
  $('cancel').disabled = readOnly || !status.dirty;
  let why = '';
  if (status.dirty && !status.check.ok) why = 'Fix the colours that do not read before applying.';
  else if (status.dirty && !draft.theme.name.trim()) why = 'Give the theme a name before applying.';
  else if (status.dirty) why = 'You have changes that are not applied yet.';
  else why = 'This is the site’s current look.';
  $('apply-note').textContent = why;
}

/** Puts the whole draft into the form and redraws. */
function fill() {
  fillThemeChoice();
  fillTheme();
  fillHeader();
  fillFooter();
  for (const slot of Object.values(SLOTS)) drawSlot(slot);
  refresh();
}

$('apply').addEventListener('click', async () => {
  note('');
  $('apply').disabled = true;
  const result = await session.apply();
  if (!result.ok) {
    if (result.error) note(messageFor(result.error) + reasonOf(result));
    refresh();
    return;
  }
  siteThemes = session.settings.themes ?? siteThemes;
  fill();
  await afterChange('The new look is applied.');
});

$('cancel').addEventListener('click', () => {
  session.cancel();
  fill();
  note('The draft was dropped.', true);
});

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

const SLOT_NAMES = { logo: 'the logo', thumbnail: 'the thumbnail', 'header.image': 'the header image', 'footer.logo': 'the footer logo' };

function drawHistory() {
  $('history-empty').hidden = history.length > 0;
  $('history').replaceChildren(
    ...history.map((entry, at) => {
      const when = formatDate(entry.at);
      const who = entry.by?.name ?? 'Someone no longer here';
      const theme = entry.look?.theme?.name ?? 'ShareCraft';
      const preview = h('button', { type: 'button', class: 'plain', 'aria-label': `Preview the look of ${when}` }, 'Preview');
      preview.addEventListener('click', async () => {
        if (!(await mayReplaceDraft(session, () => confirmDialog('Previewing this look drops the changes you have not applied. Preview it?', 'Preview')))) return;
        session.preview(entry);
        fill();
        note(`The look of ${when} is now the draft. Apply it to use it.`, true);
        $('preview-heading').scrollIntoView?.({ block: 'start' });
      });
      const restore = readOnly ? null : h('button', { type: 'button', class: 'plain', 'aria-label': `Restore the look of ${when}` }, 'Restore');
      restore?.addEventListener('click', () => restoreEntry(entry, when));
      return h(
        'li',
        {},
        h('span', { class: 'grow' }, `${when}, by ${who}`, h('br'), h('small', {}, at === 0 ? `Theme ${theme} (the current look)` : `Theme ${theme}`)),
        h('span', { class: 'actions' }, preview, restore),
      );
    }),
  );
}

async function restoreEntry(entry, when) {
  note('');
  if (!(await mayReplaceDraft(session, () => confirmDialog('Restoring drops the changes you have not applied. Restore?', 'Restore')))) return;
  const result = await session.restore(entry);
  if (result.ok) {
    siteThemes = session.settings.themes ?? siteThemes;
    fill();
    await afterChange(`The look of ${when} is applied again.`);
    return;
  }
  if (result.gone) {
    fill();
    const names = result.gone.map((slot) => SLOT_NAMES[slot] ?? slot).join(', ');
    note(`Some images of that look are no longer stored (${names}). It is loaded as a draft without them: choose new ones or apply it as it is.`);
    return;
  }
  note(messageFor(result.error) + reasonOf(result));
}

// Leaving with changes not applied asks first.
addEventListener('beforeunload', (event) => {
  if (session.status().dirty) event.preventDefault();
});

if (settingsAnswer.data.in_hub) {
  const hub = settingsAnswer.data.in_hub;
  $('hub-note').hidden = false;
  $('hub-note').replaceChildren(
    'This site is in the hub ',
    h('a', { href: pageFor('site', hub.id) }, hub.name),
    ', so its pages use the hub’s theme. The theme chosen here is kept and comes back if the site leaves the hub. The logo, header, navigation and footer are this site’s own.',
  );
}
if (!brandAnswer.ok) note(`The brand centre could not be read: ${messageFor(brandAnswer.error)}`);
showSiteThemes();
fill();
drawHistory();
