// The New site wizard and the Get started checklist (round 24), without any DOM: the wizard's
// steps and their checks, what a template gives a site (templateSummary), what Create sends,
// and the checklist a new site's owners see on its home page (checklistModel). The
// localStorage wrappers at the end never throw. new-site-wizard.js and site.js draw them.

import { kindLabel, libraryKindLabel } from './common.js';

// ---- the wizard's steps ----

/** The steps, in order: their keys and their names in the step indicator. */
export const WIZARD_STEPS = Object.freeze([
  { key: 'template', label: 'Template' },
  { key: 'details', label: 'Details' },
  { key: 'look', label: 'Look and hub' },
]);

/** The longest site name and description the server takes. */
export const NAME_MAX = 120;
export const DESCRIPTION_MAX = 500;

/**
 * The wizard at the start: on the first step, `template` chosen (the first offered, usually
 * Team site), nothing typed, the language and time zone of the template (filled in when its
 * details arrive), no theme and no hub.
 */
export function wizardState(template = 'team') {
  return { step: 0, template, name: '', description: '', language: '', timeZone: '', theme: null, hub: '', touched: false };
}

// The characters a name may not hold (as the server's validate_node_name says), the control
// characters and the invisible and direction-changing format characters.
const REFUSED = /["*:<>?/\\|]/;
// eslint-disable-next-line no-control-regex
const CONTROL = /[\u0000-\u001f\u007f-\u009f]/;
const FORMAT = /[­؀-؅؜۝܏࢐࢑࣢᠎​-‏‪-‮⁠-⁤⁦-⁯﻿]/;

/**
 * What is wrong with the site name `name`, in words, or '' when nothing is: the same rules as
 * the server (1 to 120 characters once trimmed, none of `" * : < > ? / \ |`, no control or
 * invisible characters, not ending in a dot), and not the name of a site in `taken` (names the
 * page knows, compared ignoring case; the server has the last word on names it cannot see).
 */
export function nameProblem(name, taken = []) {
  const text = String(name ?? '').trim();
  if (text === '') return 'Give the site a name.';
  if ([...text].length > NAME_MAX) return `Use ${NAME_MAX} characters or fewer.`;
  const bad = [...text].find((c) => REFUSED.test(c));
  if (bad) return `A site name cannot hold ${bad}. Leave out " * : < > ? / \\ and |.`;
  if (CONTROL.test(text) || FORMAT.test(text)) return 'The name holds an invisible character. Type it again without it.';
  if (text.endsWith('.')) return 'A site name cannot end in a dot.';
  const lower = text.toLocaleLowerCase();
  if (taken.some((other) => String(other ?? '').trim().toLocaleLowerCase() === lower)) {
    return 'There is already a site with this name. Choose a different name.';
  }
  return '';
}

/** What is wrong with the description, or ''. */
export function descriptionProblem(description) {
  return [...String(description ?? '').trim()].length > DESCRIPTION_MAX ? `Use ${DESCRIPTION_MAX} characters or fewer.` : '';
}

/**
 * The problems of step `step` (an index) of `state`, as `{ field: message }` (empty when it may
 * be left): Template needs a template, Details a good name and description. `taken` is the
 * site names the page knows.
 */
export function stepProblems(state, step = state.step, taken = []) {
  const key = WIZARD_STEPS[step]?.key;
  const problems = {};
  if (key === 'template' && !state.template) problems.template = 'Choose a template.';
  if (key === 'details') {
    const name = nameProblem(state.name, taken);
    if (name) problems.name = name;
    const description = descriptionProblem(state.description);
    if (description) problems.description = description;
  }
  return problems;
}

/** Whether the wizard may go on from its step (and Create, from the last). */
export const stepReady = (state, taken = []) => Object.keys(stepProblems(state, state.step, taken)).length === 0;

/**
 * `state` moved by `move`: `next` (only when the step has no problems; else the state marked
 * `touched`, so the page shows them), `back`, `skip` (the optional last step: its choices
 * cleared) or `{ to: index }` (back to a step already passed, never ahead of the first with a
 * problem).
 */
export function moveWizard(state, move, taken = []) {
  const last = WIZARD_STEPS.length - 1;
  if (move === 'back') return state.step > 0 ? { ...state, step: state.step - 1, touched: false } : state;
  if (move === 'next') {
    if (!stepReady(state, taken)) return { ...state, touched: true };
    return state.step < last ? { ...state, step: state.step + 1, touched: false } : state;
  }
  if (move === 'skip') return { ...state, theme: null, hub: '' };
  if (move && Number.isInteger(move.to)) {
    const to = Math.max(0, Math.min(last, move.to));
    if (to <= state.step) return { ...state, step: to, touched: false };
    for (let step = state.step; step < to; step += 1) {
      if (Object.keys(stepProblems(state, step, taken)).length) return { ...state, step, touched: true };
    }
    return { ...state, step: to, touched: false };
  }
  return state;
}

/**
 * The step indicator for `state`: per step `{ key, label, number, current, done, text }`,
 * `text` its name for screen readers ("Step 2 of 3: Details, current").
 */
export function stepperView(state) {
  const count = WIZARD_STEPS.length;
  return WIZARD_STEPS.map(({ key, label }, index) => {
    const current = index === state.step;
    const done = index < state.step;
    const status = current ? ', current' : done ? ', done' : '';
    return { key, label, number: index + 1, current, done, text: `Step ${index + 1} of ${count}: ${label}${status}` };
  });
}

/**
 * The wizard's buttons for `state`: Back (not on the first step), Next or Create (on the last),
 * Skip (on the optional last step, while something is chosen there).
 */
export function wizardButtons(state) {
  const last = state.step === WIZARD_STEPS.length - 1;
  return { back: state.step > 0, next: !last, create: last, skip: last && Boolean(state.theme || state.hub) };
}

// ---- what a template gives ----

const LIBRARY_ICON_KINDS = ['document', 'image', 'template', 'asset'];

/** How many folders `folders` (a template's, with their own `folders`) holds, at every level. */
function folderCount(folders) {
  return (Array.isArray(folders) ? folders : []).reduce((sum, folder) => sum + 1 + folderCount(folder?.folders), 0);
}

/** A navigation item of a template in words: its label, or its library's name. */
function navLabel(item) {
  const label = String(item?.label ?? '').trim();
  if (label) return label;
  return String(item?.target?.library ?? '').trim();
}

/**
 * What a card of the wizard shows for `template` (as `GET /api/templates` lists it) and its
 * package (`GET /api/templates/{id}/package`, null until it arrives or when it cannot be read):
 * `id`, `name`, `kind` and `kindLabel`, `description` (the package's, else a sentence for the
 * kind), `builtIn`, `libraries` (`[{ name, kind, icon, kindLabel, folders }]`), `navigation`
 * (the top links' labels, or null for automatic navigation), `navigationText` in words,
 * `language` and `timeZone` (the template's, '' when unknown), `known` (the package arrived)
 * and `text`, the whole card in one sentence for screen readers.
 */
export function templateSummary(template, pkg = null) {
  const body = pkg?.body ?? null;
  const kind = body?.kind ?? template?.kind ?? 'team';
  const builtIn = template?.built_in === true;
  const description =
    String(template?.description ?? pkg?.description ?? '').trim() ||
    (kind === 'communication' ? 'A site for sharing news and information with many readers.' : 'A site for a team to work on files together.');
  const libraries = (Array.isArray(body?.libraries) ? body.libraries : []).map((library) => {
    const libraryKind = LIBRARY_ICON_KINDS.includes(library?.settings?.kind) ? library.settings.kind : 'document';
    return {
      name: String(library?.name ?? ''),
      kind: libraryKind,
      icon: libraryKind,
      kindLabel: libraryKindLabel(libraryKind),
      folders: folderCount(library?.folders),
    };
  });
  const navigation = body && Array.isArray(body.navigation) ? body.navigation.map(navLabel).filter(Boolean) : null;
  const navigationText = !body
    ? ''
    : navigation === null
      ? 'Navigation: a link to each library'
      : navigation.length
        ? `Navigation: ${navigation.join(', ')}`
        : 'Navigation: none';
  const librariesText = libraries.length
    ? `${libraries.length === 1 ? '1 library' : `${libraries.length} libraries`}: ${libraries
        .map((l) => (l.folders ? `${l.name} (${l.folders === 1 ? '1 folder' : `${l.folders} folders`})` : l.name))
        .join(', ')}`
    : body
      ? 'No libraries'
      : '';
  const name = String(template?.name ?? pkg?.name ?? 'Template');
  return {
    id: String(template?.id ?? ''),
    name,
    kind,
    kindLabel: kindLabel(kind),
    description,
    builtIn,
    libraries,
    navigation,
    navigationText,
    language: String(body?.language ?? ''),
    timeZone: String(body?.time_zone ?? ''),
    known: Boolean(body),
    text: [name === kindLabel(kind) ? `${name}.` : `${name}. ${kindLabel(kind)}${builtIn ? '' : ', saved template'}.`, description, librariesText, navigationText]
      .filter(Boolean)
      .map((part) => (/[.!?]$/.test(part) ? part : `${part}.`))
      .join(' '),
  };
}

// ---- Create ----

/**
 * What Create sends for `state`: `site`, the body of `POST /api/sites` (the language and time
 * zone only when they differ from the template's `defaults`, `{ language, timeZone }`, so a
 * template keeps its own), `theme` (the chosen theme's source, or null) and `hub` (an id, or
 * null). Both are applied after the site exists.
 */
export function creationPlan(state, defaults = {}) {
  const site = { name: String(state.name ?? '').trim(), template: state.template };
  const description = String(state.description ?? '').trim();
  if (description) site.description = description;
  if (state.language && state.language !== (defaults.language ?? '')) site.language = state.language;
  if (state.timeZone && state.timeZone !== (defaults.timeZone ?? '')) site.time_zone = state.timeZone;
  return { site, theme: state.theme || null, hub: state.hub || null };
}

/** The Create button's words while it works: `site`, `look`, `hub`, or idle. */
export function createLabel(phase) {
  return { site: 'Creating the site…', look: 'Setting the look…', hub: 'Joining the hub…' }[phase] ?? 'Create site';
}

/**
 * The address of the new site `id` after Create: its home with `welcome=1`, and `unset` naming
 * the parts that could not be applied (`look`, `hub`), for the toast there.
 */
export function welcomeHref(id, unset = []) {
  const query = new URLSearchParams({ id, welcome: '1' });
  if (unset.length) query.set('unset', unset.join(','));
  return `/site.html?${query}`;
}

/** The parts named by a site page's `unset` parameter that the page knows. */
export function unsetParts(text) {
  return String(text ?? '')
    .split(',')
    .filter((part, index, all) => ['look', 'hub'].includes(part) && all.indexOf(part) === index);
}

/** The toast for parts the wizard could not apply, or '' for none. */
export function unsetText(parts) {
  if (parts.includes('look') && parts.includes('hub')) return 'The site was created, but its look and hub could not be set.';
  if (parts.includes('look')) return 'The site was created, but its look could not be set.';
  if (parts.includes('hub')) return 'The site was created, but it could not join the hub.';
  return '';
}

// ---- Get started ----

/**
 * The Get started card for `site` (as `GET /api/sites/{id}` answers it) and what the page
 * found out, `facts`: `people` (members of its groups and people or groups given access on it,
 * the owner who made it counted), `files` (whether its libraries hold any file), `lookSet`
 * (whether it has a look of its own), `hubs` (how many hubs it could join), `libraries` (how
 * many it has) and `baseline` (how many its template made; 1 when unknown). Gives `items`
 * (`[{ key, label, hint, done }]`, Join a hub only for a site in none that is not a hub, when
 * there is one to join), `done` and `total` counts, `complete`, and `summary` in words.
 */
export function checklistModel(site, facts = {}) {
  const baseline = Number.isInteger(facts.baseline) && facts.baseline >= 0 ? facts.baseline : 1;
  const items = [
    { key: 'people', label: 'Add people', hint: 'Share the site with your team.', done: (facts.people ?? 0) > 1 },
    { key: 'upload', label: 'Upload files', hint: 'Put the first files in a library.', done: facts.files === true },
    { key: 'look', label: 'Change the look', hint: 'Choose colours, a logo and a header.', done: facts.lookSet === true },
  ];
  const hubOpen = !site?.is_hub && (Boolean(site?.hub) || (facts.hubs ?? 0) > 0);
  if (hubOpen) items.push({ key: 'hub', label: 'Join a hub', hint: 'Show the hub’s navigation on this site.', done: Boolean(site?.hub) });
  items.push({ key: 'library', label: 'Create a library', hint: 'Keep different kinds of files apart.', done: (facts.libraries ?? 0) > baseline });
  const done = items.filter((item) => item.done).length;
  return {
    items,
    done,
    total: items.length,
    complete: done === items.length,
    summary: done === items.length ? 'All done. Your site is ready.' : `${done} of ${items.length} done`,
  };
}

/**
 * Whether the Get started card shows: to people who manage the site (`manages`), not on an
 * archived site, never after Dismiss; on arriving from the wizard (`welcome`), or while the
 * checklist is not complete.
 */
export function showChecklist({ manages, archived, dismissed, welcome, complete }) {
  if (!manages || archived || dismissed) return false;
  return Boolean(welcome) || !complete;
}

// ---- what the browser remembers per site and viewer ----

/** The localStorage key of the Get started state of site `siteId` for viewer `userId`. */
export const welcomeKey = (userId, siteId) => `sharecraft-get-started:${userId ?? 'anyone'}:${siteId}`;

const storage = () => {
  try {
    return globalThis.localStorage ?? null;
  } catch {
    return null;
  }
};

/** `{ dismissed, baseline }` for the site and viewer; nothing remembered reads as not dismissed, baseline null. */
export function readWelcome(userId, siteId, store = storage()) {
  try {
    const value = JSON.parse(store?.getItem(welcomeKey(userId, siteId)) ?? 'null');
    return {
      dismissed: value?.dismissed === true,
      baseline: Number.isInteger(value?.baseline) && value.baseline >= 0 ? value.baseline : null,
    };
  } catch {
    return { dismissed: false, baseline: null };
  }
}

/** Remembers `change` (`{ dismissed?, baseline? }`) over what was remembered. */
export function writeWelcome(userId, siteId, change, store = storage()) {
  try {
    store?.setItem(welcomeKey(userId, siteId), JSON.stringify({ ...readWelcome(userId, siteId, store), ...change }));
  } catch {
    // full or blocked storage: the card just shows again next time
  }
}
