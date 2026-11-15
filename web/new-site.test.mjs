import test from 'node:test';
import assert from 'node:assert/strict';
import {
  WIZARD_STEPS,
  checklistModel,
  createLabel,
  creationPlan,
  moveWizard,
  nameProblem,
  readWelcome,
  showChecklist,
  stepProblems,
  stepperView,
  templateSummary,
  unsetParts,
  unsetText,
  welcomeHref,
  welcomeKey,
  wizardButtons,
  wizardState,
  writeWelcome,
} from './new-site.js';

const memory = () => {
  const data = new Map();
  return { getItem: (k) => (data.has(k) ? data.get(k) : null), setItem: (k, v) => data.set(k, String(v)) };
};

test('nameProblem follows the server rules and the names the page knows', () => {
  assert.equal(nameProblem('  '), 'Give the site a name.');
  assert.equal(nameProblem('Sales'), '');
  assert.equal(nameProblem('x'.repeat(120)), '');
  assert.match(nameProblem('x'.repeat(121)), /120 characters/);
  assert.match(nameProblem('Sales/Marketing'), /cannot hold \//);
  assert.match(nameProblem('What?'), /cannot hold \?/);
  assert.match(nameProblem('Bad​name'), /invisible/);
  assert.match(nameProblem('Tab\there'), /invisible/);
  assert.equal(nameProblem('Ends.'), 'A site name cannot end in a dot.');
  assert.match(nameProblem(' sales ', ['Sales']), /already a site/);
  assert.equal(nameProblem('Sales 2', ['Sales']), '');
});

test('the wizard moves on only when a step is ready, and goes back freely', () => {
  let state = wizardState('team');
  assert.equal(WIZARD_STEPS.length, 3);
  assert.deepEqual(stepProblems(state), {});
  state = moveWizard(state, 'next');
  assert.equal(state.step, 1);
  const stuck = moveWizard(state, 'next');
  assert.equal(stuck.step, 1);
  assert.equal(stuck.touched, true);
  assert.deepEqual(Object.keys(stepProblems(stuck)), ['name']);
  state = moveWizard({ ...state, name: 'Sales', description: 'd'.repeat(501) }, 'next');
  assert.equal(state.step, 1);
  assert.match(stepProblems(state).description, /500/);
  state = moveWizard({ ...state, description: 'Our team' }, 'next');
  assert.equal(state.step, 2);
  assert.equal(moveWizard(state, 'next').step, 2, 'the last step stays');
  assert.equal(moveWizard(state, 'back').step, 1);
  assert.equal(moveWizard(wizardState(), 'back').step, 0);
  // a taken name stops Next
  assert.equal(moveWizard({ ...wizardState(), step: 1, name: 'Sales' }, 'next', ['sales']).step, 1);
  // jumping: back always, ahead only past ready steps
  assert.equal(moveWizard(state, { to: 0 }).step, 0);
  const ahead = moveWizard(wizardState(), { to: 2 });
  assert.equal(ahead.step, 1);
  assert.equal(ahead.touched, true);
  assert.equal(moveWizard({ ...wizardState(), name: 'Ok' }, { to: 2 }).step, 2);
  // no template
  assert.equal(moveWizard(wizardState(''), 'next').step, 0);
  // skip clears the optional choices
  const skipped = moveWizard({ ...state, theme: 'builtin:plum', hub: 'h1' }, 'skip');
  assert.equal(skipped.theme, null);
  assert.equal(skipped.hub, '');
});

test('the step indicator and buttons follow the step', () => {
  const view = stepperView({ ...wizardState(), step: 1 });
  assert.deepEqual(
    view.map((s) => [s.key, s.current, s.done]),
    [
      ['template', false, true],
      ['details', true, false],
      ['look', false, false],
    ],
  );
  assert.equal(view[1].text, 'Step 2 of 3: Details, current');
  assert.equal(view[0].text, 'Step 1 of 3: Template, done');
  assert.deepEqual(wizardButtons(wizardState()), { back: false, next: true, create: false, skip: false });
  assert.deepEqual(wizardButtons({ ...wizardState(), step: 2 }), { back: true, next: false, create: true, skip: false });
  assert.equal(wizardButtons({ ...wizardState(), step: 2, hub: 'h' }).skip, true);
});

test('templateSummary says what a template gives', () => {
  const listed = { id: 'team', name: 'Team site', description: 'For a team.', built_in: true, kind: 'team' };
  const before = templateSummary(listed);
  assert.equal(before.known, false);
  assert.deepEqual(before.libraries, []);
  assert.equal(before.navigationText, '');
  assert.equal(before.kindLabel, 'Team site');
  const team = templateSummary(listed, {
    body: { kind: 'team', language: 'en', time_zone: 'UTC', libraries: [{ name: 'Documents', folders: [] }], navigation: null },
  });
  assert.equal(team.known, true);
  assert.deepEqual(team.libraries, [{ name: 'Documents', kind: 'document', icon: 'document', kindLabel: 'Document library', folders: 0 }]);
  assert.equal(team.navigation, null);
  assert.equal(team.navigationText, 'Navigation: a link to each library');
  assert.equal(team.language, 'en');
  assert.equal(team.timeZone, 'UTC');
  assert.equal(team.text, 'Team site. For a team. 1 library: Documents. Navigation: a link to each library.');
  const saved = templateSummary(
    { id: 'abc', name: 'Project site', description: '', built_in: false, kind: 'communication' },
    {
      body: {
        kind: 'communication',
        language: 'de',
        time_zone: 'Europe/Berlin',
        libraries: [
          { name: 'Documents', folders: [{ name: 'Plans', folders: [{ name: 'Sub', folders: [] }] }, { name: 'Target', folders: [] }] },
          { name: 'Pictures', settings: { kind: 'image' }, folders: [] },
        ],
        navigation: [
          { label: '', target: { library: 'Documents' }, children: [] },
          { label: 'Help', target: { url: 'https://example.org' }, children: [] },
        ],
      },
    },
  );
  assert.equal(saved.builtIn, false);
  assert.equal(saved.kindLabel, 'Communication site');
  assert.match(saved.description, /news/);
  assert.deepEqual(
    saved.libraries.map((l) => [l.name, l.icon, l.folders]),
    [
      ['Documents', 'document', 3],
      ['Pictures', 'image', 0],
    ],
  );
  assert.deepEqual(saved.navigation, ['Documents', 'Help']);
  assert.match(saved.text, /saved template/);
  assert.match(saved.text, /2 libraries: Documents \(3 folders\), Pictures/);
  assert.equal(templateSummary(listed, { body: { kind: 'team', libraries: [], navigation: [] } }).navigationText, 'Navigation: none');
});

test('creationPlan sends only what differs from the template', () => {
  const state = { ...wizardState('abc'), name: ' Sales ', description: ' ', language: 'en', timeZone: 'Europe/London', theme: 'builtin:plum', hub: '' };
  assert.deepEqual(creationPlan(state, { language: 'en', timeZone: 'UTC' }), {
    site: { name: 'Sales', template: 'abc', time_zone: 'Europe/London' },
    theme: 'builtin:plum',
    hub: null,
  });
  assert.deepEqual(creationPlan({ ...state, description: 'Ours', theme: null, hub: 'h1', timeZone: 'UTC' }, { language: 'en', timeZone: 'UTC' }), {
    site: { name: 'Sales', template: 'abc', description: 'Ours' },
    theme: null,
    hub: 'h1',
  });
  assert.equal(createLabel('site'), 'Creating the site…');
  assert.equal(createLabel(null), 'Create site');
});

test('the welcome address carries what could not be set', () => {
  assert.equal(welcomeHref('s1'), '/site.html?id=s1&welcome=1');
  assert.equal(welcomeHref('s1', ['look', 'hub']), '/site.html?id=s1&welcome=1&unset=look%2Chub');
  assert.deepEqual(unsetParts('look,hub,nonsense,look'), ['look', 'hub']);
  assert.deepEqual(unsetParts(null), []);
  assert.match(unsetText(['look', 'hub']), /look and hub/);
  assert.match(unsetText(['hub']), /join the hub/);
  assert.equal(unsetText([]), '');
});

test('checklistModel ticks what is true', () => {
  const fresh = checklistModel({ is_hub: false, hub: null }, { people: 1, files: false, lookSet: false, hubs: 2, libraries: 1, baseline: 1 });
  assert.deepEqual(
    fresh.items.map((i) => [i.key, i.done]),
    [
      ['people', false],
      ['upload', false],
      ['look', false],
      ['hub', false],
      ['library', false],
    ],
  );
  assert.equal(fresh.summary, '0 of 5 done');
  assert.equal(fresh.complete, false);
  const busy = checklistModel({ is_hub: false, hub: { id: 'h', name: 'Hub' } }, { people: 3, files: true, lookSet: true, hubs: 0, libraries: 3, baseline: 2 });
  assert.equal(busy.complete, true);
  assert.equal(busy.summary, 'All done. Your site is ready.');
  // a hub, or no hub to join: no hub item
  assert.equal(checklistModel({ is_hub: true }, { hubs: 3 }).items.some((i) => i.key === 'hub'), false);
  assert.equal(checklistModel({ hub: null }, { hubs: 0 }).items.some((i) => i.key === 'hub'), false);
  // the template's libraries do not count; unknown baseline is one
  assert.equal(checklistModel({}, { libraries: 2, baseline: 2 }).items.at(-1).done, false);
  assert.equal(checklistModel({}, { libraries: 2 }).items.at(-1).done, true);
});

test('showChecklist: owners, until dismissed, on welcome or while not complete', () => {
  const base = { manages: true, archived: false, dismissed: false, welcome: false, complete: false };
  assert.equal(showChecklist(base), true);
  assert.equal(showChecklist({ ...base, complete: true }), false);
  assert.equal(showChecklist({ ...base, complete: true, welcome: true }), true);
  assert.equal(showChecklist({ ...base, dismissed: true, welcome: true }), false);
  assert.equal(showChecklist({ ...base, manages: false, welcome: true }), false);
  assert.equal(showChecklist({ ...base, archived: true }), false);
});

test('the welcome state is remembered per site and viewer, and never throws', () => {
  const store = memory();
  assert.deepEqual(readWelcome('u1', 's1', store), { dismissed: false, baseline: null });
  writeWelcome('u1', 's1', { baseline: 2 }, store);
  writeWelcome('u1', 's1', { dismissed: true }, store);
  assert.deepEqual(readWelcome('u1', 's1', store), { dismissed: true, baseline: 2 });
  assert.deepEqual(readWelcome('u2', 's1', store), { dismissed: false, baseline: null });
  assert.equal(welcomeKey('u1', 's1'), 'sharecraft-get-started:u1:s1');
  const broken = {
    getItem() {
      throw new Error('blocked');
    },
    setItem() {
      throw new Error('blocked');
    },
  };
  assert.deepEqual(readWelcome('u1', 's1', broken), { dismissed: false, baseline: null });
  writeWelcome('u1', 's1', { dismissed: true }, broken);
  store.setItem(welcomeKey('u3', 's1'), '{nonsense');
  assert.deepEqual(readWelcome('u3', 's1', store), { dismissed: false, baseline: null });
});
