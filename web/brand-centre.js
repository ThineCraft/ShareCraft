// The brand centre, for brand managers and administrators (everyone else signed in reads it):
// themes as a banded list with swatches (create, edit with the same live contrast check as
// Change the look, delete), brand images as a grid (add from the image picker, rename,
// remove), the default tab icon, fonts (added from the picker in font mode once the licence is
// confirmed, each with a live specimen) and font packages (a display and a content font mapped
// to the four kinds of text, with a live specimen), and, for administrators only, the limits
// for site owners and the brand managers, picked from the people. The server decides every
// change; refusals are shown as sentences.

import {
  api,
  brandImageHref,
  colourField,
  confirmDialog,
  contrastRows,
  h,
  messageFor,
  organisationChrome,
  requireMe,
  seg,
  setupTopbar,
  showMessage,
  themeSwatches,
} from './common.js';
import { mountAdminShell } from './admin-shell.js';
import { ROLES, fontForm, packageBody, policyRows, roleChoices } from './brand-fonts.js';
import { pickImage } from './image-picker.js';
import { fontSpecimen } from './look-card-view.js';
import {
  CHECK_FIELD,
  THEME_FIELDS,
  brandCentreSections,
  checkTheme,
  deriveDark,
  fontAsPackage,
  hexOf,
  packageFonts,
  parseColour,
  themeOf,
} from './look.js';
import { showToast } from './toast.js';

const $ = (id) => document.getElementById(id);
const error = $('error');
const status = $('status');
const KIND_LABELS = { logo: 'Logo', icon: 'Icon', background: 'Background' };

const me = await requireMe();
setupTopbar(me, error);
mountAdminShell({ me, current: 'brand' });
await organisationChrome();

let brand = null;
let sections = { edit: false, managers: false };
let editing = null; // the theme being edited (null: a new one)
let working = themeOf({ name: '', primary: '#3156c9', secondary: '#233239' });

const colours = {
  primary: colourField('colour-primary', 'Primary (links, buttons, focus ring)', { onInput: (v) => setColour('primary', v) }),
  secondary: colourField('colour-secondary', 'Secondary (a strong header or footer)', { onInput: (v) => setColour('secondary', v) }),
  primary_dark: colourField('colour-primary-dark', 'Primary in dark mode', { optional: true, onInput: (v) => setColour('primary_dark', v) }),
  secondary_dark: colourField('colour-secondary-dark', 'Secondary in dark mode', { optional: true, onInput: (v) => setColour('secondary_dark', v) }),
};

$('colour-fields').replaceChildren(...Object.values(colours).map((field) => field.element));

let chosen = null; // the document picked for the image being added
const FORMATS = { 'font/woff2': 'WOFF2', 'font/woff': 'WOFF', 'font/ttf': 'TTF', 'font/otf': 'OTF' };
let chosenFont = null; // the document picked for the font being added
let editingPackage = null; // the package being edited (null: a new one)
const roleSelects = {};

$('package-roles').replaceChildren(
  ...ROLES.map(([role, label]) => {
    const select = h('select', { id: `role-${role}` });
    select.addEventListener('change', drawPackageSpecimen);
    roleSelects[role] = select;
    return h('div', { class: 'field' }, h('label', { for: `role-${role}` }, label), select);
  }),
);

await load();

/** Reads the brand centre again and draws every section. */
async function load() {
  const result = await api('GET', '/brand');
  if (!result.ok) {
    showMessage(error, messageFor(result.error));
    return;
  }
  brand = result.data;
  sections = brandCentreSections(brand, me);
  $('content').hidden = false;
  $('read-only').hidden = sections.edit;
  $('new-theme').hidden = !sections.edit;
  $('add-image').hidden = !sections.edit;
  $('add-font').hidden = !sections.edit;
  $('new-package').hidden = !sections.edit;
  $('managers-section').hidden = !sections.managers;
  $('limits-section').hidden = !me.is_admin;
  drawThemes();
  drawImages();
  drawIcon();
  drawFonts();
  drawPackages();
  if (me.is_admin) drawLimits();
  if (sections.managers) await drawManagers();
}

/** Shows `text` in the message paragraph `id`; `ok` for a confirmation rather than a refusal. */
function note(id, text, ok = false) {
  $(id).classList.toggle('ok', ok);
  showMessage($(id), text);
}

/** The sentence for a refused change, with the server's reason when it gave one. */
function refusal(result) {
  const reason = result.data?.reason;
  return messageFor(result.error) + (typeof reason === 'string' && reason ? ` (${reason})` : '');
}

// ---- themes ----

function drawThemes() {
  $('themes').replaceChildren(
    ...brand.themes.map((theme) => {
      const kind = theme.builtin ? 'Built in' : 'Organisation';
      const actions = [];
      if (sections.edit && !theme.builtin) {
        const edit = h('button', { type: 'button', class: 'plain', 'aria-label': `Edit the theme ${theme.name}` }, 'Edit');
        edit.addEventListener('click', () => openThemeForm(theme));
        const remove = h('button', { type: 'button', class: 'plain', 'aria-label': `Delete the theme ${theme.name}` }, 'Delete');
        remove.addEventListener('click', () => deleteTheme(theme));
        actions.push(edit, remove);
      }
      return h(
        'li',
        {},
        themeSwatches(theme),
        h('span', { class: 'grow' }, theme.name, h('br'), h('small', {}, kind)),
        actions.length ? h('span', { class: 'actions' }, ...actions) : null,
      );
    }),
  );
}

function setColour(field, value) {
  const text = String(value ?? '').trim();
  if (text === '' && field.endsWith('_dark')) delete working[field];
  else working[field] = parseColour(text) ? text.toLowerCase() : text;
  showDerived();
  checkForm();
}

function showDerived() {
  const derived = (light, role) => {
    const colour = parseColour(light ?? '');
    const dark = colour ? deriveDark(colour, role) : null;
    return dark ? hexOf(dark) : '';
  };
  if (!working.primary_dark) colours.primary_dark.set('', derived(working.primary, 'primary'));
  if (!working.secondary_dark) colours.secondary_dark.set('', derived(working.secondary, 'secondary'));
}

function fillForm() {
  $('theme-name').value = working.name;
  for (const field of THEME_FIELDS) colours[field].set(working[field] ?? '');
  showDerived();
  checkForm();
}

/** The live check: the failing pairs with "Use suggested", swatches, and Save while all pass. */
function checkForm() {
  const check = checkTheme(working);
  $('pairs').replaceChildren(
    ...contrastRows(check, (pair) => {
      if (typeof pair.suggestion !== 'string') return;
      const field = CHECK_FIELD[pair.check];
      working[field] = pair.suggestion;
      fillForm();
      colours[field].input.focus();
    }),
  );
  const shown = check.ok ? { resolved: check.resolved } : working;
  $('theme-swatches').replaceChildren(...themeSwatches(shown).childNodes);
  $('save-theme').disabled = !check.ok || working.name.trim() === '';
}

function openThemeForm(theme) {
  editing = theme;
  working = themeOf(theme ?? { name: '', primary: '#3156c9', secondary: '#233239' });
  $('theme-form-heading').textContent = theme ? `Edit ${theme.name}` : 'New theme';
  $('theme-form').hidden = false;
  note('theme-msg', '');
  fillForm();
  $('theme-name').focus();
}

$('new-theme').addEventListener('click', () => openThemeForm(null));
$('cancel-theme').addEventListener('click', () => {
  $('theme-form').hidden = true;
  editing = null;
});
$('theme-name').addEventListener('input', () => {
  working.name = $('theme-name').value;
  checkForm();
});

$('theme-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  note('theme-msg', '');
  const body = {
    name: working.name.trim(),
    primary: working.primary,
    secondary: working.secondary,
    primary_dark: working.primary_dark ?? null,
    secondary_dark: working.secondary_dark ?? null,
  };
  const result = editing
    ? await api('PATCH', `/brand/themes/${seg(editing.id)}`, body)
    : await api('POST', '/brand/themes', body);
  if (!result.ok) return note('theme-msg', refusal(result));
  $('theme-form').hidden = true;
  status.textContent = editing ? `Saved the theme ${result.data.name}.` : `Added the theme ${result.data.name}.`;
  editing = null;
  await load();
});

async function deleteTheme(theme) {
  status.textContent = '';
  const sure = await confirmDialog(`Delete the theme ${theme.name}? Sites that use it keep their own copy.`, 'Delete theme');
  if (!sure) return;
  const result = await api('DELETE', `/brand/themes/${seg(theme.id)}`);
  if (!result.ok) return showMessage(error, refusal(result));
  status.textContent = `Deleted the theme ${theme.name}.`;
  await load();
}

// ---- brand images ----


function drawImages() {
  $('images-empty').hidden = brand.images.length > 0;
  $('images').replaceChildren(
    ...brand.images.map((image) => {
      const picture = h('span', { class: 'thumb', 'aria-hidden': 'true' });
      const url = brandImageHref(image.url);
      if (url) {
        const img = h('img', { src: url, alt: '' });
        img.addEventListener('error', () => picture.replaceChildren(h('span', { class: 'placeholder' }, 'Not available')));
        picture.append(img);
      }
      const actions = [];
      if (sections.edit) {
        const rename = h('button', { type: 'button', class: 'plain', 'aria-label': `Rename ${image.name}` }, 'Rename');
        rename.addEventListener('click', () => renameImage(image));
        const remove = h('button', { type: 'button', class: 'plain', 'aria-label': `Remove ${image.name}` }, 'Remove');
        remove.addEventListener('click', () => removeImage(image));
        actions.push(rename, remove);
      }
      return h(
        'li',
        {},
        picture,
        h('span', { class: 'tile-name' }, image.name),
        h('span', { class: 'tile-foot' }, h('small', { class: 'mark' }, KIND_LABELS[image.kind] ?? image.kind), ...actions),
      );
    }),
  );
}

$('add-image').addEventListener('click', async () => {
  note('images-msg', '');
  const picked = await pickImage({ title: 'Choose a brand image' });
  if (!picked?.doc) return;
  chosen = picked;
  $('image-chosen').textContent = picked.name;
  $('image-name').value = picked.name;
  $('image-form').hidden = false;
  $('image-kind').focus();
});

$('cancel-image').addEventListener('click', () => {
  chosen = null;
  $('image-form').hidden = true;
});

$('image-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  if (!chosen) return;
  note('images-msg', '');
  const result = await api('POST', '/brand/images', {
    doc: chosen.doc,
    kind: $('image-kind').value,
    name: $('image-name').value.trim(),
  });
  if (!result.ok) return note('images-msg', refusal(result));
  chosen = null;
  $('image-form').hidden = true;
  status.textContent = `Added ${result.data.name}.`;
  await load();
});

/** Asks for a new name in a dialog: resolves the text, or null for Cancel or Escape. */
function askName(label, value) {
  return new Promise((resolve) => {
    const input = h('input', { type: 'text', id: 'rename-input', maxlength: '120', autocomplete: 'off', required: true });
    input.value = value;
    const cancel = h('button', { type: 'button', class: 'plain' }, 'Cancel');
    const save = h('button', { type: 'submit' }, 'Rename');
    const form = h('form', { method: 'dialog' }, h('h2', {}, label), h('div', { class: 'field' }, h('label', { for: 'rename-input' }, 'Name'), input), h('div', { class: 'row end' }, cancel, save));
    const dialog = h('dialog', { 'aria-label': label }, form);
    let answered = false;
    const answer = (text) => {
      if (answered) return;
      answered = true;
      if (dialog.open) dialog.close();
      dialog.remove();
      resolve(text);
    };
    form.addEventListener('submit', (event) => {
      event.preventDefault();
      answer(input.value.trim());
    });
    cancel.addEventListener('click', () => answer(null));
    dialog.addEventListener('close', () => answer(null));
    document.body.append(dialog);
    dialog.showModal();
    input.select();
  });
}

async function renameImage(image) {
  note('images-msg', '');
  const name = await askName(`Rename ${image.name}`, image.name);
  if (name === null || name === image.name) return;
  const result = await api('PATCH', `/brand/images/${seg(image.id)}`, { name });
  if (!result.ok) return note('images-msg', result.error === 'invalid_request' ? 'Give the image a name of 1 to 120 characters, without control characters.' : refusal(result));
  status.textContent = `Renamed to ${result.data.name}.`;
  await load();
}

async function removeImage(image) {
  note('images-msg', '');
  const sure = await confirmDialog(`Remove ${image.name} from the brand centre? Sites that use it keep it.`, 'Remove image');
  if (!sure) return;
  const result = await api('DELETE', `/brand/images/${seg(image.id)}`);
  if (!result.ok) return note('images-msg', refusal(result));
  status.textContent = `Removed ${image.name}.`;
  await load();
}

// ---- brand managers (administrators only) ----

async function drawManagers() {
  const managers = brand.managers ?? [];
  $('managers-empty').hidden = managers.length > 0;
  $('managers').replaceChildren(
    ...managers.map((person) => {
      const remove = h('button', { type: 'button', class: 'plain', 'aria-label': `Remove ${person.name} as a brand manager` }, 'Remove');
      remove.addEventListener('click', () =>
        setManagers(
          managers.filter((m) => m.id !== person.id).map((m) => m.id),
          `${person.name} is no longer a brand manager.`,
        ),
      );
      return h('li', {}, h('span', { class: 'grow' }, person.name, h('br'), h('small', {}, person.email)), remove);
    }),
  );
  const people = await api('GET', '/admin/users');
  const choices = people.ok ? people.data.filter((u) => !u.disabled && !managers.some((m) => m.id === u.id)) : [];
  $('manager-choice').replaceChildren(...choices.map((u) => h('option', { value: u.id }, `${u.name} (${u.email})`)));
  $('add-manager').hidden = choices.length === 0;
  if (!people.ok) note('managers-msg', messageFor(people.error));
}

$('add-manager').addEventListener('submit', async (event) => {
  event.preventDefault();
  const id = $('manager-choice').value;
  if (!id) return;
  const option = $('manager-choice').selectedOptions[0];
  const ids = [...(brand.managers ?? []).map((m) => m.id), id];
  await setManagers(ids, `${option?.textContent ?? 'They'} is now a brand manager.`);
});

async function setManagers(ids, done) {
  note('managers-msg', '');
  const result = await api('PUT', '/brand/managers', { users: ids });
  if (!result.ok) return note('managers-msg', refusal(result));
  status.textContent = done;
  await load();
}

// ---- the default tab icon ----

/** The default icon as a tile, and for brand managers a choice among the brand images of kind Icon. */
function drawIcon() {
  const icons = brand.images.filter((image) => image.kind === 'icon');
  const current = brand.images.find((image) => image.id === brand.default_icon) ?? null;
  const picture = h('span', { class: 'slot-picture', 'aria-hidden': 'true' });
  const url = brandImageHref(brand.default_icon_url);
  if (url) {
    const img = h('img', { src: url, alt: '' });
    img.addEventListener('error', () => (img.hidden = true));
    picture.append(img);
  } else {
    picture.append(h('span', { class: 'slot-empty' }, 'ShareCraft’s own icon'));
  }
  const parts = [picture, h('span', { class: 'slot-label' }, 'Default tab icon'), h('span', { class: 'slot-name' }, current?.name ?? 'None')];
  if (sections.edit) {
    const select = h('select', { id: 'icon-choice', disabled: icons.length === 0, 'aria-describedby': 'icon-why' });
    select.replaceChildren(
      h('option', { value: '' }, 'None (ShareCraft’s own icon)'),
      ...icons.map((image) => h('option', { value: image.id }, image.name)),
    );
    select.value = current?.id ?? '';
    const save = h('button', { type: 'button', class: 'secondary small', disabled: true }, 'Use this icon');
    select.addEventListener('change', () => (save.disabled = select.value === (current?.id ?? '')));
    save.addEventListener('click', () => setDefaultIcon(select.value || null));
    const why = icons.length
      ? 'Square PNG or SVG brand images of the kind Icon can be the default.'
      : 'Add a brand image of the kind Icon first: a square PNG or SVG.';
    parts.push(
      h('div', { class: 'field' }, h('label', { for: 'icon-choice' }, 'Icon'), select),
      h('span', { class: 'slot-why', id: 'icon-why' }, why),
      h('span', { class: 'slot-actions' }, save),
    );
  }
  $('icon-slot').replaceChildren(h('div', { class: 'slot card' }, ...parts));
}

async function setDefaultIcon(id) {
  note('icon-msg', '');
  const result = await api('PUT', '/brand/default-icon', { image: id });
  if (!result.ok) return note('icon-msg', refusal(result));
  showToast(id ? 'The default tab icon is changed.' : 'Pages use ShareCraft’s own icon again.', { kind: 'ok' });
  await load();
}

// ---- fonts ----

/** The date a licence was confirmed, in words. */
function confirmedText(licence) {
  const at = licence?.confirmed_at;
  const when = typeof at === 'string' || typeof at === 'number' ? new Date(typeof at === 'number' ? at * 1000 : at) : null;
  return when && !Number.isNaN(when.getTime()) ? `Licence confirmed ${when.toLocaleDateString()}` : 'Licence confirmed';
}

function drawFonts() {
  const fonts = brand.fonts ?? [];
  $('fonts-empty').hidden = fonts.length > 0;
  $('fonts').replaceChildren(
    ...fonts.map((font) => {
      const actions = [];
      if (sections.edit) {
        const remove = h('button', { type: 'button', class: 'plain', 'aria-label': `Remove the font ${font.name}` }, 'Remove');
        remove.addEventListener('click', () => removeFont(font));
        actions.push(remove);
      }
      return h(
        'li',
        {},
        h('span', { class: 'grow' }, font.name, h('br'), h('small', {}, `${FORMATS[font.media_type] ?? 'Font'} · ${confirmedText(font.licence)}`)),
        fontSpecimen(packageFonts(fontAsPackage(font))),
        actions.length ? h('span', { class: 'actions' }, ...actions) : null,
      );
    }),
  );
}

/** Add font stays off until the form is complete, and says why. */
function checkFontForm() {
  const state = fontForm({ chosen: chosenFont, name: $('font-name').value, confirmed: $('font-licence').checked });
  $('save-font').disabled = !state.canAdd;
  $('font-why').textContent = state.reason;
  return state;
}

$('add-font').addEventListener('click', async () => {
  note('fonts-msg', '');
  const picked = await pickImage({ title: 'Choose a font file', mode: 'font' });
  if (!picked?.doc) return;
  chosenFont = picked;
  $('font-chosen').textContent = picked.name;
  $('font-name').value = picked.name.replace(/\.(woff2?|ttf|otf)$/i, '');
  $('font-licence').checked = false;
  $('font-form-specimen').replaceChildren(h('p', { class: 'meta' }, 'A specimen shows here once the font is added.'));
  $('font-form').hidden = false;
  checkFontForm();
  $('font-name').focus();
});

$('font-name').addEventListener('input', checkFontForm);
$('font-licence').addEventListener('change', checkFontForm);
$('cancel-font').addEventListener('click', () => {
  chosenFont = null;
  $('font-form').hidden = true;
});

$('font-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  const state = checkFontForm();
  if (!state.canAdd) return;
  note('fonts-msg', '');
  const result = await api('POST', '/brand/fonts', state.body);
  if (!result.ok) return note('fonts-msg', refusal(result));
  chosenFont = null;
  $('font-form').hidden = true;
  showToast(`Added the font ${result.data.name}.`, { kind: 'ok' });
  await load();
});

async function removeFont(font) {
  note('fonts-msg', '');
  const sure = await confirmDialog(`Remove the font ${font.name}? Packages that use it lose it, and their text falls back to the system fonts. Sites keep the copy their look holds.`, 'Remove font');
  if (!sure) return;
  const result = await api('DELETE', `/brand/fonts/${seg(font.id)}`);
  if (!result.ok) return note('fonts-msg', refusal(result));
  showToast(`Removed the font ${font.name}.`, { kind: 'ok' });
  await load();
}

// ---- font packages ----

/** The fonts of a package in words: "Display: X · Content: Y", or the system fonts. */
function packageWords(pkg) {
  const parts = [];
  if (pkg.display) parts.push(`Display: ${pkg.display.name}`);
  if (pkg.content) parts.push(`Content: ${pkg.content.name}`);
  if (!parts.length) parts.push(Object.values(pkg.roles ?? {}).includes('system-serif') ? 'System serif and sans-serif' : 'System fonts');
  return parts.join(' · ');
}

function drawPackages() {
  $('packages').replaceChildren(
    ...(brand.font_packages ?? []).map((pkg) => {
      const actions = [];
      if (sections.edit && !pkg.builtin) {
        const edit = h('button', { type: 'button', class: 'plain', 'aria-label': `Edit the font package ${pkg.name}` }, 'Edit');
        edit.addEventListener('click', () => openPackageForm(pkg));
        const remove = h('button', { type: 'button', class: 'plain', 'aria-label': `Delete the font package ${pkg.name}` }, 'Delete');
        remove.addEventListener('click', () => deletePackage(pkg));
        actions.push(edit, remove);
      }
      return h(
        'li',
        {},
        h('span', { class: 'grow' }, pkg.name, h('br'), h('small', {}, `${pkg.builtin ? 'Built in' : 'Organisation'} · ${packageWords(pkg)}`)),
        fontSpecimen(packageFonts(pkg)),
        actions.length ? h('span', { class: 'actions' }, ...actions) : null,
      );
    }),
  );
}

/** The package as the form holds it. */
function packageFormState() {
  const roles = {};
  for (const [role] of ROLES) roles[role] = roleSelects[role].value;
  return { name: $('package-name').value, display: $('package-display').value, content: $('package-content').value, roles };
}

/** The role choices for the fonts chosen: a font the package lacks is offered disabled, saying why. */
function fillRoleChoices() {
  const has = { display: Boolean($('package-display').value), content: Boolean($('package-content').value) };
  for (const [role] of ROLES) {
    const select = roleSelects[role];
    const value = select.value;
    select.replaceChildren(
      ...roleChoices(has).map((choice) =>
        h('option', { value: choice.value, disabled: choice.disabled }, choice.disabled ? `${choice.label} (${choice.reason.replace(/\.$/, '').toLowerCase()})` : choice.label),
      ),
    );
    const usable = roleChoices(has).find((c) => c.value === value && !c.disabled);
    select.value = usable ? value : 'system';
  }
}

/** The live specimen of the form: title, heading, body and button, each in its role's font. */
function drawPackageSpecimen() {
  const body = packageBody(packageFormState());
  const fontOf = (choice) => (choice ? ((brand.fonts ?? []).find((f) => f.id === choice.brand_font) ?? null) : null);
  const fonts = packageFonts({ display: fontOf(body.display), content: fontOf(body.content), roles: body.roles });
  $('package-specimen').replaceChildren(fontSpecimen(fonts));
}

function fillFontSelect(select, chosen) {
  select.replaceChildren(h('option', { value: '' }, 'None'), ...(brand.fonts ?? []).map((font) => h('option', { value: font.id }, font.name)));
  select.value = chosen ?? '';
}

function openPackageForm(pkg) {
  editingPackage = pkg;
  $('package-form-heading').textContent = pkg ? `Edit ${pkg.name}` : 'New font package';
  $('package-name').value = pkg?.name ?? '';
  // A package's fonts are listed by name; find each among the brand centre's fonts by its address.
  const idOf = (font) => (font ? ((brand.fonts ?? []).find((f) => f.url === font.url)?.id ?? '') : '');
  fillFontSelect($('package-display'), idOf(pkg?.display));
  fillFontSelect($('package-content'), idOf(pkg?.content));
  for (const [role] of ROLES) roleSelects[role].value = pkg?.roles?.[role] ?? 'system';
  fillRoleChoices();
  for (const [role] of ROLES) {
    const wanted = pkg?.roles?.[role];
    if (wanted && [...roleSelects[role].options].some((o) => o.value === wanted && !o.disabled)) roleSelects[role].value = wanted;
  }
  drawPackageSpecimen();
  note('package-msg', '');
  $('package-form').hidden = false;
  $('package-name').focus();
}

for (const id of ['package-display', 'package-content']) {
  $(id).addEventListener('change', () => {
    fillRoleChoices();
    drawPackageSpecimen();
  });
}
$('new-package').addEventListener('click', () => openPackageForm(null));
$('cancel-package').addEventListener('click', () => {
  editingPackage = null;
  $('package-form').hidden = true;
});

$('package-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  note('package-msg', '');
  const body = packageBody(packageFormState());
  if (!body.name) return note('package-msg', 'Give the package a name.');
  const result = editingPackage
    ? await api('PATCH', `/brand/font-packages/${seg(editingPackage.id)}`, body)
    : await api('POST', '/brand/font-packages', body);
  if (!result.ok) return note('package-msg', refusal(result));
  $('package-form').hidden = true;
  showToast(editingPackage ? `Saved the font package ${result.data.name}.` : `Added the font package ${result.data.name}.`, { kind: 'ok' });
  editingPackage = null;
  await load();
});

async function deletePackage(pkg) {
  note('packages-msg', '');
  const sure = await confirmDialog(`Delete the font package ${pkg.name}? Sites that use it keep their own copy.`, 'Delete package');
  if (!sure) return;
  const result = await api('DELETE', `/brand/font-packages/${seg(pkg.id)}`);
  if (!result.ok) return note('packages-msg', refusal(result));
  showToast(`Deleted the font package ${pkg.name}.`, { kind: 'ok' });
  await load();
}

// ---- limits for site owners (administrators only) ----

/** The five switches, each with what it means; Save while something changed. */
function drawLimits() {
  const rows = policyRows(brand.policy);
  $('limits').replaceChildren(
    ...rows.map((row) => {
      const input = h('input', { type: 'checkbox', id: `limit-${row.key}`, 'data-key': row.key, 'aria-describedby': `limit-${row.key}-note` });
      input.checked = row.checked;
      input.addEventListener('change', checkLimits);
      return h(
        'li',
        {},
        h('label', { class: 'check', for: `limit-${row.key}` }, input, ` ${row.label}`),
        h('span', { class: 'meta', id: `limit-${row.key}-note` }, row.explanation),
      );
    }),
  );
  checkLimits();
}

/** The policy the switches hold. */
function limitsChosen() {
  const out = {};
  for (const input of $('limits').querySelectorAll('input[data-key]')) out[input.dataset.key] = input.checked;
  return out;
}

function checkLimits() {
  const saved = Object.fromEntries(policyRows(brand.policy).map((row) => [row.key, row.checked]));
  const chosen = limitsChosen();
  const changed = Object.keys(chosen).some((key) => chosen[key] !== saved[key]);
  $('save-limits').disabled = !changed;
  $('limits-why').textContent = changed ? 'You have changes that are not saved yet.' : 'No changes to save.';
}

$('limits-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  note('limits-msg', '');
  const result = await api('PUT', '/brand/policy', limitsChosen());
  if (!result.ok) return note('limits-msg', refusal(result));
  showToast('The limits for site owners are saved. They apply to the next change of each site’s look.', { kind: 'ok' });
  await load();
});
