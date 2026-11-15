// The New site wizard on the start page (round 24): a modal dialog (a full-screen sheet on
// phones) in three steps, Template, Details and Look and hub, with a step indicator and Back,
// Next, Skip and Create. The state and its checks are in new-site.js; this draws them.

import { LANGUAGES, api, fillSelect, h, languageChoices, messageFor, seg, timeZoneChoices } from './common.js';
import { draftFromLook, lookInput, themeOf } from './look.js';
import { themeGallery } from './look-studio.js';
import {
  WIZARD_STEPS,
  createLabel,
  creationPlan,
  moveWizard,
  nameProblem,
  stepProblems,
  stepperView,
  templateSummary,
  welcomeHref,
  wizardButtons,
  wizardState,
  writeWelcome,
  descriptionProblem,
} from './new-site.js';
import { libraryIcon } from './site-home.js';

const $ = (id) => document.getElementById(id);

/** The server's refusals of a new site that belong to a field of the Details step. */
const FIELD_OF_ERROR = {
  invalid_name: 'name',
  name_taken: 'name',
  invalid_description: 'description',
  invalid_setting: 'description',
  invalid_language: 'regional',
  invalid_time_zone: 'regional',
};

/**
 * Sets up the wizard for `me` (an administrator): `opener` opens it, focus goes back there when
 * it closes. Answers `open()`.
 */
export function setupNewSiteWizard(me, opener) {
  const dialog = $('new-site-dialog');
  let state = wizardState();
  let templates = []; // as GET /api/templates lists them
  const packages = new Map(); // template id -> package, or null when it could not be read
  let brandThemes = null; // GET /api/brand's themes, once read
  let hubs = null; // GET /api/hubs, once read
  let taken = []; // the names of the sites the page knows
  let regionalFor = null; // the template whose language and time zone the fields hold
  let busy = false;
  let serverProblems = {}; // refusals of the last Create, by field

  const summaryOf = (id) => {
    const template = templates.find((t) => t.id === id) ?? { id, name: id, kind: id === 'communication' ? 'communication' : 'team', built_in: true };
    return templateSummary(template, packages.get(id) ?? null);
  };

  // ---- loading what the steps show ----

  async function loadTemplates() {
    const result = await api('GET', '/templates');
    templates = result.ok && result.data.length
      ? result.data
      : [
          { id: 'team', name: 'Team site', description: '', built_in: true, kind: 'team' },
          { id: 'communication', name: 'Communication site', description: '', built_in: true, kind: 'communication' },
        ];
    if (!templates.some((t) => t.id === state.template)) state.template = templates[0].id;
    drawTemplates();
    // What each template holds arrives card by card; a package that cannot be read leaves the card without its preview.
    await Promise.all(
      templates.map(async (template) => {
        if (packages.has(template.id)) return;
        const answer = await api('GET', `/templates/${seg(template.id)}/package`);
        packages.set(template.id, answer.ok ? answer.data : null);
        refreshCard(template.id);
      }),
    );
  }

  async function loadTaken() {
    const result = await api('GET', '/sites');
    if (result.ok) taken = result.data.map((site) => site.name);
  }

  async function loadLookChoices() {
    if (brandThemes && hubs) return;
    const [brand, hubList] = await Promise.all([brandThemes ? null : api('GET', '/brand'), hubs ? null : api('GET', '/hubs')]);
    if (!brandThemes) brandThemes = brand?.ok ? (brand.data.themes ?? []) : [];
    if (!hubs) hubs = hubList?.ok ? hubList.data : [];
    drawGallery();
    drawHubs();
  }

  // ---- Step 1: the template cards ----

  function drawTemplates() {
    $('template-cards').replaceChildren(...templates.map(templateCard));
  }

  function templateCard(template) {
    const id = `template-card-${template.id}`;
    const input = h('input', {
      type: 'radio',
      name: 'site-template',
      value: template.id,
      id,
      class: 'pick-input',
      'aria-describedby': `${id}-text`,
    });
    input.checked = template.id === state.template;
    input.addEventListener('change', () => {
      if (!input.checked) return;
      state = { ...state, template: template.id };
      showMessage('template-msg', '');
    });
    const card = h('label', { class: 'template-card', for: id, 'data-template': template.id }, input);
    fillCard(card, summaryOf(template.id));
    return card;
  }

  function refreshCard(templateId) {
    const card = $('template-cards').querySelector(`[data-template="${CSS.escape(templateId)}"]`);
    if (card) fillCard(card, summaryOf(templateId));
    if (templateId === state.template && state.step === 1) fillRegional(true);
  }

  /** The card's words and its small picture of the site: navigation links over library rows. */
  function fillCard(card, summary) {
    const input = card.querySelector('input');
    const navigation = summary.known
      ? h(
          'span',
          { class: 'tp-nav' },
          ...(summary.navigation === null ? summary.libraries.map((l) => l.name) : summary.navigation)
            .slice(0, 4)
            .map((label) => h('span', { class: 'tp-link' }, label)),
        )
      : null;
    const libraries = summary.known
      ? h(
          'span',
          { class: 'tp-libraries' },
          ...summary.libraries.slice(0, 4).map((library) =>
            h(
              'span',
              { class: 'tp-library' },
              libraryIcon(library.icon),
              h('span', { class: 'tp-library-name' }, library.name),
              library.folders ? h('span', { class: 'tp-folders' }, library.folders === 1 ? '1 folder' : `${library.folders} folders`) : null,
            ),
          ),
          summary.libraries.length > 4 ? h('span', { class: 'tp-more' }, `and ${summary.libraries.length - 4} more`) : null,
        )
      : null;
    const preview = h(
      'span',
      { class: `tp-preview${summary.known ? '' : ' loading'}`, 'aria-hidden': 'true' },
      ...(summary.known
        ? [navigation, libraries]
        : [h('span', { class: 'tp-wait' }, packages.has(summary.id) ? 'Contents not available' : 'Reading what it holds…')]),
    );
    card.replaceChildren(
      input,
      h(
        'span',
        { class: 'tp-head' },
        h('span', { class: 'tp-name' }, summary.name),
        h('span', { class: 'tp-kind' }, summary.builtIn ? (summary.name === summary.kindLabel ? 'Built in' : `${summary.kindLabel} · Built in`) : `${summary.kindLabel} · Saved template`),
      ),
      h('span', { class: 'tp-description' }, summary.description),
      preview,
      h('span', { class: 'visually-hidden', id: `${input.id}-text` }, summary.text),
    );
  }

  // ---- Step 2: details ----

  const languageName = (tag) => LANGUAGES.find(([value]) => value === tag)?.[1] ?? tag;

  /** Puts the template's language and time zone in the fields (again when the template changed, or `force`). */
  function fillRegional(force = false) {
    if (regionalFor === state.template && !force) return;
    const summary = summaryOf(state.template);
    regionalFor = state.template;
    // Until the template's own arrive, the organisation's (a built-in template's) stand in.
    const org = packages.get('team')?.body;
    const language = summary.language || org?.language || 'en';
    const zone = summary.timeZone || org?.time_zone || 'UTC';
    fillSelect($('site-language'), languageChoices(language), language);
    fillSelect($('site-zone'), timeZoneChoices(zone), zone);
    state = { ...state, language, timeZone: zone };
    $('regional-note').textContent =
      summary.builtIn || !org
        ? 'Your organisation’s language and time zone. Dates, times and sizes on the site are shown in them.'
        : `As the template ${summary.name} sets them${org.language !== language || org.time_zone !== zone ? `; your organisation uses ${languageName(org.language)} and ${org.time_zone}` : ''}.`;
    showRegionalSummary();
  }

  function showRegionalSummary() {
    $('regional-summary').textContent = `${languageName(state.language)}, ${state.timeZone}`;
  }

  $('site-language').addEventListener('change', () => {
    state = { ...state, language: $('site-language').value };
    delete serverProblems.regional;
    showRegionalSummary();
    showDetailsProblems();
  });
  $('site-zone').addEventListener('change', () => {
    state = { ...state, timeZone: $('site-zone').value };
    delete serverProblems.regional;
    showRegionalSummary();
    showDetailsProblems();
  });

  let nameLeft = false; // the person left the name field once: an empty name is then said too
  $('site-name').addEventListener('input', () => {
    state = { ...state, name: $('site-name').value };
    delete serverProblems.name;
    showDetailsProblems();
  });
  $('site-name').addEventListener('blur', () => {
    if ($('site-name').value.trim() !== '') nameLeft = true;
    showDetailsProblems();
  });
  $('site-description').addEventListener('input', () => {
    state = { ...state, description: $('site-description').value };
    delete serverProblems.description;
    showDetailsProblems();
  });

  /** The messages under the fields: the rules as you type, an empty name once asked, the server's refusals. */
  function showDetailsProblems() {
    const showEmpty = state.touched || nameLeft;
    const name = serverProblems.name ?? (state.name.trim() === '' && !showEmpty ? '' : nameProblem(state.name, taken));
    const description = serverProblems.description ?? descriptionProblem(state.description);
    fieldProblem('site-name', 'site-name-msg', name);
    fieldProblem('site-description', 'site-description-msg', description);
    const regional = serverProblems.regional ?? '';
    showMessage('regional-msg', regional);
    if (regional) $('regional').open = true;
  }

  function fieldProblem(fieldId, messageId, text) {
    showMessage(messageId, text);
    if (text) $(fieldId).setAttribute('aria-invalid', 'true');
    else $(fieldId).removeAttribute('aria-invalid');
  }

  // ---- Step 3: look and hub ----

  function drawGallery() {
    const card = (model, title, sub) => {
      const id = `wizard-theme-${(model.from || 'default').replace(/[^a-z0-9-]/gi, '-')}`;
      const input = h('input', { type: 'radio', name: 'wizard-theme', value: model.from, id, class: 'pick-input', 'aria-describedby': `${id}-colours` });
      input.checked = (state.theme ?? '') === model.from;
      input.addEventListener('change', () => {
        if (input.checked) state = { ...state, theme: model.from || null };
        drawButtons();
      });
      const mini = (scheme, colour) => {
        const page = h('span', { class: 'tc-mini', 'data-scheme': scheme, 'aria-hidden': 'true' }, h('span', { class: 'tc-band' }), h('span', { class: 'tc-line' }), h('span', { class: 'tc-button' }));
        if (colour) {
          page.style.setProperty('--tc-primary', colour.primary);
          page.style.setProperty('--tc-secondary', colour.secondary);
        }
        return page;
      };
      return h(
        'label',
        { class: `theme-card${model.from ? '' : ' default'}`, for: id },
        input,
        h('span', { class: 'tc-pages' }, mini('light', model.light), mini('dark', model.dark)),
        h('span', { class: 'tc-name' }, title ?? model.name),
        sub ? h('span', { class: 'tc-sub' }, sub) : null,
        h(
          'span',
          { class: 'visually-hidden', id: `${id}-colours` },
          model.light ? `Light mode: primary ${model.light.primary}, secondary ${model.light.secondary}. Dark mode: primary ${model.dark.primary}, secondary ${model.dark.secondary}.` : 'The colours the site has until you change them.',
        ),
      );
    };
    const groups = themeGallery(brandThemes ?? [], []).map(({ label, cards }) =>
      h('div', { class: 'theme-group' }, h('h4', { class: 'theme-group-label' }, label), h('div', { class: 'theme-cards' }, ...cards.map((model) => card(model)))),
    );
    const keep = h('div', { class: 'theme-group' }, h('div', { class: 'theme-cards' }, card({ from: '', name: 'Default' }, 'Default colours', 'Leave the look as it is')));
    $('wizard-gallery').replaceChildren(keep, ...groups);
  }

  function drawHubs() {
    const list = hubs ?? [];
    $('wizard-hub-field').hidden = list.length === 0;
    $('wizard-hub').replaceChildren(h('option', { value: '' }, 'No hub'), ...list.map((hub) => h('option', { value: hub.id }, hub.name)));
    $('wizard-hub').value = state.hub;
  }

  $('wizard-hub').addEventListener('change', () => {
    state = { ...state, hub: $('wizard-hub').value };
    drawButtons();
  });

  // ---- the steps, the indicator and the buttons ----

  function drawStepper() {
    $('wizard-steps').replaceChildren(
      ...stepperView(state).map((step, index) => {
        const mark = h('span', { class: 'ws-number', 'aria-hidden': 'true' }, step.done ? '✓' : String(step.number));
        const words = h('span', { class: 'ws-label' }, step.label);
        const spoken = h('span', { class: 'visually-hidden' }, step.text);
        // A step already passed is a button back to it.
        const inner = step.done && !busy
          ? (() => {
              const button = h('button', { type: 'button', class: 'ws-button' }, mark, words, spoken);
              button.addEventListener('click', () => go({ to: index }));
              return button;
            })()
          : h('span', { class: 'ws-button' }, mark, words, spoken);
        // The visible words are hidden from screen readers where the full sentence says them.
        words.setAttribute('aria-hidden', 'true');
        return h(
          'li',
          { class: `ws${step.current ? ' current' : ''}${step.done ? ' done' : ''}`, 'aria-current': step.current ? 'step' : null },
          inner,
        );
      }),
    );
  }

  function drawButtons() {
    const buttons = wizardButtons(state);
    $('wizard-back').hidden = !buttons.back;
    $('wizard-next').hidden = !buttons.next;
    $('new-site-submit').hidden = !buttons.create;
    $('wizard-skip').hidden = !buttons.skip;
  }

  /** Shows the wizard's step; `focus` puts focus on the step's heading (after a move). */
  function drawStep(focus = false) {
    WIZARD_STEPS.forEach(({ key }, index) => {
      $(`step-${key}`).hidden = index !== state.step;
    });
    drawStepper();
    drawButtons();
    const key = WIZARD_STEPS[state.step].key;
    if (key === 'details') {
      const summary = summaryOf(state.template);
      $('details-template').textContent = summary.name === summary.kindLabel ? `Template: ${summary.name}.` : `Template: ${summary.name} (${summary.kindLabel}).`;
      fillRegional();
      showDetailsProblems();
    }
    if (key === 'look') loadLookChoices();
    if (key === 'template') showMessage('template-msg', state.touched ? (stepProblems(state).template ?? '') : '');
    const view = stepperView(state)[state.step];
    $('wizard-live').textContent = view.text.replace(', current', '');
    if (focus) {
      // Back on Details with a problem goes to that field; otherwise to the step's heading.
      const field = key === 'details' && state.touched ? firstProblemField() : null;
      (field ?? $(`step-${key}-title`)).focus();
    }
  }

  function firstProblemField() {
    if ($('site-name').hasAttribute('aria-invalid')) return $('site-name');
    if ($('site-description').hasAttribute('aria-invalid')) return $('site-description');
    if (serverProblems.regional) return $('site-language');
    return null;
  }

  function go(move) {
    const before = state.step;
    state = moveWizard(state, move, taken);
    drawStep(true);
    if (state.step === before && state.touched && WIZARD_STEPS[state.step].key === 'details') firstProblemField()?.focus();
  }

  $('wizard-next').addEventListener('click', () => go('next'));
  $('wizard-back').addEventListener('click', () => go('back'));
  $('wizard-skip').addEventListener('click', () => {
    state = moveWizard(state, 'skip');
    drawGallery();
    drawHubs();
    drawButtons();
    $('step-look-title').focus();
    $('wizard-live').textContent = 'No look or hub chosen.';
  });
  // Enter in a field of an earlier step goes on, as Next does; on the last it creates the site.
  $('new-site').addEventListener('submit', (event) => {
    event.preventDefault();
    if (busy) return;
    if (state.step < WIZARD_STEPS.length - 1) return go('next');
    create();
  });

  // ---- Create ----

  function setBusy(phase) {
    busy = Boolean(phase);
    const submit = $('new-site-submit');
    submit.textContent = createLabel(phase);
    submit.disabled = busy;
    submit.setAttribute('aria-busy', String(busy));
    submit.classList.toggle('working', busy);
    for (const id of ['wizard-back', 'wizard-skip', 'new-site-cancel', 'new-site-close']) $(id).disabled = busy;
    for (const input of $('step-look').querySelectorAll('input, select')) input.disabled = busy;
    drawStepper();
  }

  async function create() {
    // Every step is checked again: the Details may have been left with a problem by going back.
    for (let step = 0; step < WIZARD_STEPS.length; step += 1) {
      if (Object.keys(stepProblems(state, step, taken)).length) {
        state = { ...state, step, touched: true };
        drawStep(true);
        return;
      }
    }
    showMessage('new-site-error', '');
    const summary = summaryOf(state.template);
    const plan = creationPlan(state, { language: summary.language, timeZone: summary.timeZone });
    setBusy('site');
    const made = await api('POST', '/sites', plan.site);
    if (!made.ok) {
      setBusy(null);
      const field = FIELD_OF_ERROR[made.error];
      if (field) {
        serverProblems = { [field]: made.error === 'name_taken' ? 'There is already a site with this name. Choose a different name.' : messageFor(made.error) };
        state = { ...state, step: 1, touched: true };
        drawStep(true);
      } else {
        showMessage('new-site-error', messageFor(made.error));
        $('new-site-submit').focus();
      }
      return;
    }
    const siteId = made.data.id;
    // The Get started list counts libraries beyond the ones the template made.
    if (summary.known) writeWelcome(me.id, siteId, { baseline: summary.libraries.length });
    const unset = [];
    // The look and the hub go on through their own routes; a refusal there does not undo the site.
    if (plan.theme) {
      setBusy('look');
      const theme = (brandThemes ?? []).find((t) => t.from === plan.theme);
      let ok = false;
      if (theme) {
        const draft = draftFromLook(null);
        draft.theme = themeOf(theme);
        draft.from = theme.from;
        const result = await api('PUT', `/sites/${seg(siteId)}/look`, { look: lookInput(draft, { themes: brandThemes }) });
        ok = result.ok;
      }
      if (!ok) unset.push('look');
    }
    if (plan.hub) {
      setBusy('hub');
      const result = await api('PUT', `/sites/${seg(siteId)}/joined-hub`, { hub: plan.hub });
      if (!result.ok) unset.push('hub');
    }
    location.assign(welcomeHref(siteId, unset));
  }

  // ---- opening and closing ----

  dialog.addEventListener('cancel', (event) => {
    if (busy) event.preventDefault();
  });
  $('new-site-cancel').addEventListener('click', () => dialog.close());
  $('new-site-close').addEventListener('click', () => dialog.close());
  dialog.addEventListener('close', () => opener.focus());

  function open() {
    state = wizardState(templates[0]?.id ?? 'team');
    serverProblems = {};
    nameLeft = false;
    regionalFor = null;
    $('site-name').value = '';
    $('site-description').value = '';
    $('regional').open = false;
    showMessage('new-site-error', '');
    setBusy(null);
    drawTemplates();
    drawStep();
    dialog.showModal();
    const checked = $('template-cards').querySelector('input:checked') ?? $('template-cards').querySelector('input');
    (checked ?? $('step-template-title')).focus();
    if (!templates.length) ready.then(() => dialog.open && $('template-cards').querySelector('input:checked')?.focus());
    loadTaken();
  }

  const ready = loadTemplates();
  return { open };
}

function showMessage(id, text) {
  document.getElementById(id).textContent = text;
}
