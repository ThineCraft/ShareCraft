import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import { checkTheme, contrast, deriveDark, hexOf, lookTokens, parseColour } from './look.js';

// The same table the Rust tests read (crates/revisions/src/brand.rs).
const vectors = JSON.parse(readFileSync(new URL('./look-vectors.json', import.meta.url), 'utf8'));
const round2 = (x) => Math.round(x * 100) / 100;

test('contrast_matches_shared_vectors', () => {
  assert.ok(vectors.contrast.length > 10);
  for (const { a, b, ratio } of vectors.contrast) {
    assert.equal(round2(contrast(a, b)), ratio, `${a} on ${b}`);
    assert.equal(round2(contrast(b, a)), ratio, `${b} on ${a}`);
  }
  assert.equal(contrast('#000000', '#ffffff'), 21);
});

test('derive_dark_matches_shared_vectors', () => {
  for (const { from, role, dark } of vectors.derive_dark) {
    assert.equal(hexOf(deriveDark(from, role)), dark, `${role} ${from}`);
  }
});

test('built_in_themes_pass_every_check', () => {
  assert.equal(vectors.built_in_themes.length, 6);
  for (const t of vectors.built_in_themes) {
    const result = checkTheme({ primary: t.primary, secondary: t.secondary });
    assert.equal(result.ok, true, t.id);
    assert.deepEqual(result.resolved, t.resolved, t.id);
    // The worse of the two dark page colours is the dark paper: 4.51 to 4.70 for these themes.
    const dark = parseColour(result.resolved.dark.accent);
    const ratio = Math.min(contrast(dark, '#202a2e'), contrast(dark, '#141c20'));
    assert.ok(ratio >= 4.5 && ratio <= 4.7, `${t.id} dark accent: ${ratio}`);
  }
});

test('every_check_on_every_path', () => {
  const names = new Set();
  for (const { note, theme, pairs } of vectors.failing_themes) {
    const result = checkTheme(theme);
    assert.equal(result.ok, false, note);
    assert.deepEqual(result.pairs, pairs, note);
    for (const p of result.pairs) names.add(p.check);
  }
  // Five checks fail in these cases; the sixth is covered below.
  assert.deepEqual(
    [...names].sort(),
    [
      'button_on_primary',
      'header_on_secondary',
      'header_on_secondary_dark',
      'primary_dark_on_dark',
      'primary_on_light',
    ],
  );
  // button_on_primary_dark never fails beside a passing primary_dark_on_dark: a colour that
  // reads on the dark paper always carries the dark ink. So a dark accent that fails only the
  // paper check shows just that pair.
  const dim = checkTheme({
    primary: '#3156c9',
    secondary: '#233239',
    primary_dark: '#3a4a52',
  });
  assert.deepEqual(
    dim.pairs.map((p) => p.check),
    ['primary_dark_on_dark'],
  );
});

test('colour_parsing_refuses_anything_but_hex', () => {
  assert.deepEqual(parseColour('#3156C9'), { r: 0x31, g: 0x56, b: 0xc9 });
  assert.equal(hexOf(parseColour('#3156C9')), '#3156c9');
  for (const bad of [
    'red',
    '#fff',
    '3156c9',
    '#3156c9 ',
    ' #3156c9',
    '#3156c9ff',
    '#3156cg',
    'red;background:url(x)',
    '#3156c9;x',
    'rgb(1,2,3)',
    '',
    null,
    undefined,
    42,
  ]) {
    assert.equal(parseColour(bad), null, String(bad));
  }
  assert.equal(checkTheme({ primary: 'red', secondary: '#233239' }).ok, false);
  assert.equal(checkTheme({ primary: '#3156c9', secondary: '#233239', primary_dark: 'x' }).ok, false);
});

test('look_tokens_use_light_dark', () => {
  const effective = {
    theme: checkTheme({ primary: '#3156c9', secondary: '#233239' }).resolved,
  };
  assert.deepEqual(lookTokens(effective), {
    '--accent': 'light-dark(#3156c9,#748ddd)',
    '--accent-ink': 'light-dark(#3156c9,#748ddd)',
    '--on-accent': 'light-dark(#ffffff,#141c20)',
    '--header-bg': 'light-dark(#233239,#233239)',
    '--header-ink': 'light-dark(#ffffff,#ffffff)',
  });
});

// ---- applying a look to the page ----

import { applyLook } from './look.js';

/** An element whose style only records `setProperty` and `removeProperty`; markup throws. */
function rootElement() {
  const set = {};
  const removed = [];
  return {
    set,
    removed,
    style: {
      setProperty(name, value) {
        set[name] = value;
      },
      removeProperty(name) {
        removed.push(name);
        delete set[name];
      },
    },
    setAttribute() {
      throw new Error('no attribute is ever set');
    },
    set innerHTML(_) {
      throw new Error('innerHTML is never used');
    },
  };
}

const SHARECRAFT = checkTheme({ primary: '#3156c9', secondary: '#233239' }).resolved;

test('apply_look_sets_only_known_variables', () => {
  const root = rootElement();
  const effective = {
    theme: {
      light: { ...SHARECRAFT.light, extra: '#000000' },
      dark: { ...SHARECRAFT.dark, extra: '#000000' },
      '--bg': '#000000',
    },
    '--text': '#000000',
  };
  assert.equal(applyLook(root, effective), true);
  assert.deepEqual(root.set, {
    '--accent': 'light-dark(#3156c9,#748ddd)',
    '--accent-ink': 'light-dark(#3156c9,#748ddd)',
    '--on-accent': 'light-dark(#ffffff,#141c20)',
    '--header-bg': 'light-dark(#233239,#233239)',
    '--header-ink': 'light-dark(#ffffff,#ffffff)',
  });
  for (const never of ['--bg', '--paper', '--text', '--muted', '--line', '--ok', '--warn-bg', '--stop-ink']) {
    assert.ok(!(never in root.set), never);
  }
});

test('apply_look_refuses_non_hex', () => {
  for (const bad of ['red;background:url(x)', 'red', '#3156c', '#3156c9;--bg:red', 'url(x)', '', null, 7]) {
    const root = rootElement();
    // An earlier, good look is taken away: the page falls back to its own colours.
    applyLook(root, { theme: SHARECRAFT });
    const effective = { theme: { light: { ...SHARECRAFT.light, header_bg: bad }, dark: SHARECRAFT.dark } };
    assert.equal(applyLook(root, effective), false, String(bad));
    assert.deepEqual(root.set, {}, String(bad));
    assert.ok(root.removed.includes('--accent'));
  }
  // No theme at all (a look that could not be read) and no root are refused quietly.
  assert.equal(applyLook(rootElement(), null), false);
  assert.equal(applyLook(rootElement(), { theme: null }), false);
  assert.equal(applyLook(null, { theme: SHARECRAFT }), false);
  assert.equal(applyLook({}, { theme: SHARECRAFT }), false);
});

// ---- Change the look: the draft, the live check, history and the brand centre ----

import {
  brandCentreSections,
  draftFromLook,
  lookInput,
  lookSession,
  previewEffective,
} from './look.js';
import { filterImages, imageEntries } from './image-picker.js';

const SITE_ID = '11111111-1111-4111-8111-111111111111';
const DOC_A = '0000000a-0000-4000-8000-00000000000a';
const DOC_B = '0000000b-0000-4000-8000-00000000000b';
const SHA_A = 'a'.repeat(64);
const SHA_B = 'b'.repeat(64);
const SHA_C = 'c'.repeat(64);
const ref = (sha256, from_doc, name) => ({ sha256, media_type: 'image/png', bytes: 10, width: 4, height: 4, name, from_doc });
const STORED = Object.freeze({
  theme: { name: 'ShareCraft', primary: '#3156c9', secondary: '#233239' },
  from: 'builtin:sharecraft',
  logo: ref(SHA_A, DOC_A, 'Logo.png'),
  logo_alt: '',
  header: { layout: 'compact', background: 'none', align: 'left', hide_title: false },
  navigation: 'horizontal',
  footer: { enabled: false, layout: 'simple', name: '', background: 'none', groups: [] },
});
const settingsOf = (look) => ({ look: structuredClone(look), themes: [], theme_source_changed: false });
const THEMES = [
  { id: 'builtin:sharecraft', from: 'builtin:sharecraft', name: 'ShareCraft', primary: '#3156c9', secondary: '#233239', primary_dark: null, secondary_dark: null },
  { id: 'builtin:plum', from: 'builtin:plum', name: 'Plum', primary: '#7a3e8f', secondary: '#3b2244', primary_dark: null, secondary_dark: null },
];

/** A stand-in for `api`: records every call and answers with `answer(method, path, body)`. */
function sender(answer = () => ({ ok: true, status: 200, data: settingsOf(STORED) })) {
  const calls = [];
  const send = async (method, path, body) => {
    calls.push({ method, path, body });
    return answer(method, path, body);
  };
  return { calls, send };
}

test('draft_changes_never_saved_until_apply', async () => {
  const { calls, send } = sender((method, path, body) => ({
    ok: true,
    status: 200,
    data: { look: { ...STORED, header: { ...STORED.header, layout: body.look.header.layout } }, themes: [], theme_source_changed: false },
  }));
  const session = lookSession({ siteId: SITE_ID, settings: settingsOf(STORED), themes: THEMES, brandImages: [], send });
  assert.equal(session.status().dirty, false);
  assert.equal(session.status().canApply, false, 'nothing to apply yet');
  session.change((draft) => {
    draft.header.layout = 'standard';
  });
  session.change((draft) => {
    draft.navigation = 'vertical';
  });
  session.chooseTheme(THEMES[1]);
  assert.equal(calls.length, 0, 'changing the draft sends nothing');
  assert.equal(session.status().dirty, true);
  // Cancel drops the draft, still without a call.
  session.cancel();
  assert.equal(session.status().dirty, false);
  assert.equal(session.draft.header.layout, 'compact');
  assert.equal(session.draft.from, 'builtin:sharecraft');
  assert.equal(calls.length, 0);
  // Apply sends the draft once, as a look.
  session.change((draft) => {
    draft.header.layout = 'standard';
  });
  assert.equal(session.status().canApply, true);
  const result = await session.apply();
  assert.equal(result.ok, true);
  assert.equal(calls.length, 1);
  assert.equal(calls[0].method, 'PUT');
  assert.equal(calls[0].path, `/sites/${SITE_ID}/look`);
  assert.equal(calls[0].body.look.header.layout, 'standard');
  assert.deepEqual(calls[0].body.look.theme, { from: 'builtin:sharecraft' });
  assert.deepEqual(calls[0].body.look.logo, { keep: SHA_A }, 'an image the look holds is kept');
  // The saved look is now what the server answered; the draft starts from it.
  assert.equal(session.status().dirty, false);
  assert.equal(session.draft.header.layout, 'standard');
  // A read-only (archived) site never applies.
  const archived = lookSession({ siteId: SITE_ID, settings: settingsOf(STORED), themes: THEMES, brandImages: [], send, readOnly: true });
  archived.change((draft) => {
    draft.navigation = 'vertical';
  });
  assert.equal(archived.status().canApply, false);
  assert.equal((await archived.apply()).ok, false);
  assert.equal(calls.length, 1);
});

test('apply_disabled_while_contrast_fails', async () => {
  const { calls, send } = sender();
  const session = lookSession({ siteId: SITE_ID, settings: settingsOf(STORED), themes: THEMES, brandImages: [], send });
  session.setColour('primary', '#FFFF00');
  assert.equal(session.draft.theme.primary, '#ffff00', 'colours are kept in lower case');
  assert.equal(session.draft.from, 'custom', 'an edited colour makes the theme custom');
  const status = session.status();
  assert.equal(status.dirty, true);
  assert.equal(status.check.ok, false);
  assert.ok(status.check.pairs.some((p) => p.check === 'primary_on_light'));
  assert.equal(status.canApply, false);
  assert.equal((await session.apply()).ok, false);
  assert.equal(calls.length, 0, 'nothing is sent while a pair fails');
  // A colour that is not #rrggbb cannot be applied either.
  session.setColour('primary', 'red;background:url(x)');
  assert.equal(session.status().canApply, false);
  assert.equal(session.status().check.invalid, true);
  // A readable colour again: Apply comes back.
  session.setColour('primary', '#1f5f8b');
  assert.equal(session.status().canApply, true);
  // A dark colour left empty is derived.
  session.setColour('primary_dark', '');
  assert.equal('primary_dark' in session.draft.theme, false);
});

test('use_suggested_fixes_the_pair', () => {
  const { send } = sender();
  const session = lookSession({ siteId: SITE_ID, settings: settingsOf(STORED), themes: THEMES, brandImages: [], send });
  const cases = [
    ['primary', '#7fa0ff', 'primary_on_light'],
    ['secondary', '#777777', 'header_on_secondary'],
    ['secondary_dark', '#808080', 'header_on_secondary_dark'],
    ['primary_dark', '#3a4a52', 'primary_dark_on_dark'],
  ];
  for (const [field, colour, check] of cases) {
    session.cancel();
    session.setColour(field, colour);
    const pair = session.status().check.pairs.find((p) => p.check === check);
    assert.ok(pair, `${check} fails for ${colour}`);
    assert.match(pair.suggestion, /^#[0-9a-f]{6}$/);
    session.useSuggested(pair);
    assert.equal(session.draft.theme[field], pair.suggestion, check);
    const after = session.status().check;
    assert.ok(!(after.pairs ?? []).some((p) => p.check === check), `${check} fixed`);
    assert.equal(after.ok, true, check);
    assert.equal(session.status().canApply, true, check);
  }
  // A pair without a suggestion (a derived dark colour that cannot pass) changes nothing.
  const before = structuredClone(session.draft);
  session.useSuggested({ check: 'primary_dark_on_dark', ratio: 1.2, suggestion: null });
  assert.deepEqual(session.draft, before);
});

test('history_entry_loads_as_draft', () => {
  const { calls, send } = sender();
  const session = lookSession({ siteId: SITE_ID, settings: settingsOf(STORED), themes: THEMES, brandImages: [], send });
  const older = {
    ...structuredClone(STORED),
    theme: { name: 'Plum', primary: '#7a3e8f', secondary: '#3b2244' },
    from: 'builtin:plum',
    logo: ref(SHA_B, DOC_B, 'Old logo.png'),
    header: { layout: 'extended', background: 'strong', align: 'centre', hide_title: false },
  };
  session.preview({ event_id: 7, at: '2026-11-14T10:00:00Z', by: null, look: older });
  assert.deepEqual(session.draft, draftFromLook(older));
  assert.equal(session.status().dirty, true);
  assert.equal(calls.length, 0, 'a preview sends nothing');
  // The preview shows it: the theme's colours, its logo through the brand image address.
  const effective = previewEffective(session.draft, 'Sales');
  assert.equal(effective.theme.light.accent, '#7a3e8f');
  assert.equal(effective.logo_url, `/api/brand/images/${SHA_B}`);
  assert.equal(effective.logo_alt, 'Sales');
  assert.equal(effective.header.layout, 'extended');
  // An entry with no look is the default look.
  session.preview({ event_id: 1, look: null });
  assert.equal(session.draft.from, 'builtin:sharecraft');
  assert.equal(session.draft.logo, null);
  // Applying a previewed look keeps its exact image snapshots, never re-taking them from their
  // documents, even when the brand centre holds the same content.
  const input = lookInput(draftFromLook(older), { themes: THEMES });
  assert.deepEqual(input.theme, { from: 'builtin:plum' });
  assert.deepEqual(input.logo, { keep: SHA_B });
  assert.ok(!JSON.stringify(input).includes(DOC_B), 'no image is chosen from its document again');
  // An image replaced in the picker is sent as chosen.
  const replaced = draftFromLook(older);
  replaced.logo = { doc: DOC_A, name: 'New.png' };
  replaced.thumbnail = { brand_image: 'img-1', sha256: SHA_C, name: 'Mark' };
  const chosen = lookInput(replaced, { themes: THEMES });
  assert.deepEqual(chosen.logo, { doc: DOC_A });
  assert.deepEqual(chosen.thumbnail, { brand_image: 'img-1' });
  // A theme changed since in the brand centre is sent as the copy the draft holds.
  const changed = lookInput(draftFromLook(older), { themes: [{ ...THEMES[1], primary: '#6a2e7f' }] });
  assert.deepEqual(changed.theme, { name: 'Plum', primary: '#7a3e8f', secondary: '#3b2244' });
});

test('image_gone_loads_without_images', async () => {
  const { calls, send } = sender(() => ({
    ok: false,
    status: 409,
    error: 'image_gone',
    data: { error: 'image_gone', slots: ['logo', 'header.image'] },
  }));
  const session = lookSession({ siteId: SITE_ID, settings: settingsOf(STORED), themes: THEMES, brandImages: [], send });
  const older = {
    ...structuredClone(STORED),
    logo: ref(SHA_B, DOC_B, 'Gone.png'),
    thumbnail: ref(SHA_C, DOC_A, 'Mark.png'),
    header: { layout: 'extended', background: 'image', image: ref(SHA_B, DOC_B, 'Band.png'), align: 'left', hide_title: true },
  };
  const result = await session.restore({ event_id: 12, look: older });
  assert.equal(calls.length, 1);
  assert.deepEqual(calls[0], { method: 'POST', path: `/sites/${SITE_ID}/look/rollback`, body: { event_id: 12 } });
  assert.equal(result.ok, false);
  assert.deepEqual(result.gone, ['logo', 'header.image']);
  // The entry is now the draft, without the images whose content is gone.
  const draft = session.draft;
  assert.equal(draft.logo, null);
  assert.equal(draft.header.image, null);
  assert.equal(draft.header.background, 'none', 'an image band without its image has no background');
  assert.equal(draft.header.hide_title, false, 'the title shows without a logo');
  assert.equal(draft.thumbnail.sha256, SHA_C, 'the other images stay');
  assert.equal(session.status().dirty, true);
  // A restore that works makes the answer the saved look.
  const ok = sender(() => ({ ok: true, status: 200, data: settingsOf({ ...STORED, navigation: 'vertical' }) }));
  const other = lookSession({ siteId: SITE_ID, settings: settingsOf(STORED), themes: THEMES, brandImages: [], send: ok.send });
  assert.equal((await other.restore({ event_id: 3, look: STORED })).ok, true);
  assert.equal(other.draft.navigation, 'vertical');
  assert.equal(other.status().dirty, false);
});

test('picker_filters_and_lists_only_images', () => {
  const library = { id: 'lib-1', name: 'Brand assets' };
  const items = [
    { id: 'f1', kind: 'folder', name: 'Logos.png' },
    { id: 'd1', kind: 'document', name: 'Logo.PNG', thumbnail: true },
    { id: 'd2', kind: 'document', name: 'Mark.svg' },
    { id: 'd3', kind: 'document', name: 'Report.docx' },
    { id: 'd4', kind: 'document', name: 'Band.jpeg', thumbnail: true },
    { id: 'd5', kind: 'document', name: 'Held.png', scan: 'held' },
    { id: 'd6', kind: 'document', name: 'Bad.webp', scan: 'quarantined' },
    { id: 'd7', kind: 'document', name: 'Blocked.gif', policy: 'blocked' },
    { id: 'd8', kind: 'document', name: 'notes.svg.txt' },
    { id: 'd9', kind: 'document', name: 'Photo.avif' },
    { id: 'd10', kind: 'document', name: 'Banner.webp', thumbnail: true },
  ];
  const entries = imageEntries(items, library);
  assert.deepEqual(
    entries.map((e) => e.name),
    ['Band.jpeg', 'Banner.webp', 'Logo.PNG', 'Mark.svg'],
  );
  assert.deepEqual(entries[2], {
    doc: 'd1',
    name: 'Logo.PNG',
    library: 'Brand assets',
    thumb: '/api/docs/d1/thumbnail',
  });
  assert.equal(entries[3].thumb, '/api/docs/d2/preview', 'an SVG is shown from its preview');
  assert.deepEqual(filterImages(entries, '  ban ').map((e) => e.name), ['Band.jpeg', 'Banner.webp']);
  assert.deepEqual(filterImages(entries, 'LOGO').map((e) => e.name), ['Logo.PNG']);
  assert.deepEqual(filterImages(entries, 'brand').map((e) => e.name), entries.map((e) => e.name), 'the library name matches too');
  assert.deepEqual(filterImages(entries, ''), entries);
  assert.deepEqual(filterImages(entries, 'zzz'), []);
});

test('brand_centre_hides_managers_from_non_admins', () => {
  const managers = [{ id: 'u1', name: 'Mia', email: 'mia@example.test' }];
  // A brand manager changes themes and images but never sees or names the managers.
  assert.deepEqual(brandCentreSections({ can_manage: true }, { is_admin: false }), { edit: true, managers: false });
  assert.deepEqual(brandCentreSections({ can_manage: true, managers }, { is_admin: false }), { edit: true, managers: false });
  // An administrator sees them.
  assert.deepEqual(brandCentreSections({ can_manage: true, managers }, { is_admin: true }), { edit: true, managers: true });
  // Anyone else signed in reads the brand centre only.
  assert.deepEqual(brandCentreSections({ can_manage: false }, { is_admin: false }), { edit: false, managers: false });
});

test('look_input_sends_only_what_the_layout_uses', () => {
  const draft = draftFromLook({
    ...structuredClone(STORED),
    logo: null,
    header: { layout: 'compact', background: 'image', image: ref(SHA_A, DOC_A, 'Band.png'), align: 'right', hide_title: true },
    footer: {
      enabled: true,
      layout: 'simple',
      name: 'Sales',
      background: 'soft',
      groups: [
        { heading: 'Kept out', links: [{ label: 'Home', url: '/' }, { label: ' ', url: '' }] },
        { heading: 'Second', links: [{ label: 'Help', url: 'https://example.test/' }] },
      ],
    },
  });
  const input = lookInput(draft, { current: STORED, themes: THEMES });
  // A compact header has no image, alignment or image background; no logo, no hidden title.
  assert.deepEqual(input.header, { layout: 'compact', background: 'none', align: 'left', hide_title: false });
  // A simple footer: the first group, no heading, empty links left out.
  assert.deepEqual(input.footer.groups, [{ heading: '', links: [{ label: 'Home', url: '/' }] }]);
  // Extended: the image band and alignment go, and every group with its heading.
  draft.header.layout = 'extended';
  draft.footer.layout = 'extended';
  const wide = lookInput(draft, { current: STORED, themes: THEMES });
  assert.deepEqual(wide.header.image, { keep: SHA_A });
  assert.equal(wide.header.background, 'image');
  assert.equal(wide.header.align, 'right');
  assert.deepEqual(
    wide.footer.groups.map((g) => g.heading),
    ['Kept out', 'Second'],
  );
  // An empty group added to edit is no change.
  const { send } = sender();
  const session = lookSession({ siteId: SITE_ID, settings: settingsOf(STORED), themes: THEMES, brandImages: [], send });
  session.change((d) => d.footer.groups.push({ heading: '', links: [{ label: '', url: '' }] }));
  assert.equal(session.status().dirty, false);
});

import { mayReplaceDraft } from './look.js';

test('history_preview_asks_before_dropping_changes', async () => {
  const { send } = sender();
  const session = lookSession({ siteId: SITE_ID, settings: settingsOf(STORED), themes: THEMES, brandImages: [], send });
  const entry = { event_id: 4, look: { ...structuredClone(STORED), navigation: 'vertical' } };
  let asked = 0;
  const answer = (yes) => async () => {
    asked += 1;
    return yes;
  };
  // Nothing unapplied: no question.
  assert.equal(await mayReplaceDraft(session, answer(false)), true);
  assert.equal(asked, 0);
  // Unapplied changes: asked, and kept when the person says no.
  session.change((draft) => (draft.header.layout = 'standard'));
  assert.equal(await mayReplaceDraft(session, answer(false)), false);
  assert.equal(asked, 1);
  assert.equal(session.draft.header.layout, 'standard', 'the draft is untouched');
  // Replaced only after a yes.
  assert.equal(await mayReplaceDraft(session, answer(true)), true);
  session.preview(entry);
  assert.equal(session.draft.navigation, 'vertical');
});

test('image_gone_draft_applied_as_it_is_keeps_its_other_snapshots', async () => {
  const answers = [
    { ok: false, status: 409, error: 'image_gone', data: { error: 'image_gone', slots: ['logo'] } },
    { ok: true, status: 200, data: settingsOf(STORED) },
  ];
  const { calls, send } = sender(() => answers.shift());
  const session = lookSession({ siteId: SITE_ID, settings: settingsOf(STORED), themes: THEMES, brandImages: [], send });
  const older = { ...structuredClone(STORED), logo: ref(SHA_B, DOC_B, 'Gone.png'), thumbnail: ref(SHA_C, DOC_A, 'Mark.png') };
  await session.restore({ event_id: 9, look: older });
  assert.equal((await session.apply()).ok, true);
  const look = calls[1].body.look;
  assert.equal(look.logo, undefined, 'the gone image is left out');
  assert.deepEqual(look.thumbnail, { keep: SHA_C }, 'the other snapshot is kept, not re-taken');
});
