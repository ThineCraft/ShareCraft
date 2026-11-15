// The brand centre, for brand managers and administrators (everyone else signed in reads it):
// themes as a banded list with swatches (create, edit with the same live contrast check as
// Change the look, delete), brand images as a grid (add from the image picker, rename,
// remove), and, for administrators only, the brand managers, picked from the people. The server
// decides every change; refusals are shown as sentences.

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
import { pickImage } from './image-picker.js';
import { CHECK_FIELD, THEME_FIELDS, brandCentreSections, checkTheme, deriveDark, hexOf, parseColour, themeOf } from './look.js';

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
  $('managers-section').hidden = !sections.managers;
  drawThemes();
  drawImages();
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

let editing = null; // the theme being edited (null: a new one)
let working = themeOf({ name: '', primary: '#3156c9', secondary: '#233239' });

const colours = {
  primary: colourField('colour-primary', 'Primary (links, buttons, focus ring)', { onInput: (v) => setColour('primary', v) }),
  secondary: colourField('colour-secondary', 'Secondary (a strong header or footer)', { onInput: (v) => setColour('secondary', v) }),
  primary_dark: colourField('colour-primary-dark', 'Primary in dark mode', { optional: true, onInput: (v) => setColour('primary_dark', v) }),
  secondary_dark: colourField('colour-secondary-dark', 'Secondary in dark mode', { optional: true, onInput: (v) => setColour('secondary_dark', v) }),
};
$('colour-fields').replaceChildren(...Object.values(colours).map((field) => field.element));

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

let chosen = null; // the document picked for the image being added

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
