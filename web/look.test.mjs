import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

import { BLEED_DEFAULT } from './bleed.js';
import {
  checkTheme,
  contrast,
  deriveDark,
  FONT_VARIABLES,
  fontTokens,
  hexOf,
  lookTokens,
  parseColour,
  SYSTEM,
  SYSTEM_SERIF,
} from './look.js';

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
  assert.equal(vectors.built_in_themes.length, 7);
  for (const t of vectors.built_in_themes) {
    const theme = { primary: t.primary, secondary: t.secondary };
    if (t.primary_dark) theme.primary_dark = t.primary_dark;
    if (t.secondary_dark) theme.secondary_dark = t.secondary_dark;
    const result = checkTheme(theme);
    assert.equal(result.ok, true, t.id);
    assert.deepEqual(result.resolved, t.resolved, t.id);
    // A theme with its own dark colours is not derived, so it need not sit at the edge.
    if (t.primary_dark) continue;
    // The worse of the two dark page colours is the dark paper: 4.51 to 4.70 for derived themes.
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
  // With the image band goes its bleed, at the defaults unless changed.
  assert.deepEqual(wide.header.bleed, BLEED_DEFAULT);
  draft.header.bleed = { ...BLEED_DEFAULT, enabled: false, distance: 999, tint_dark: '#ABCDEF' };
  assert.deepEqual(lookInput(draft, { current: STORED, themes: THEMES }).header.bleed, { ...BLEED_DEFAULT, enabled: false, distance: 400, tint_dark: '#abcdef' });
  // The part of the image shown goes with the image band, in range; none until one is chosen.
  assert.equal(wide.header.frame, undefined);
  draft.header.frame = { x: 1200, y: 250, zoom: 150 };
  assert.deepEqual(lookInput(draft, { current: STORED, themes: THEMES }).header.frame, { x: 1000, y: 250, zoom: 150 });
  draft.header.layout = 'compact';
  assert.equal(lookInput(draft, { current: STORED, themes: THEMES }).header.frame, undefined);
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

import { DEFAULT_LOOK, describeLookChange } from './look.js';

test('describe_look_change_says_what_changed', () => {
  const before = { ...structuredClone(STORED), logo: null };
  const after = {
    ...structuredClone(STORED),
    theme: { name: 'Plum', primary: '#7a3e8f', secondary: '#3b2244' },
    from: 'builtin:plum',
    logo: ref(SHA_A, DOC_A, 'Logo.png'),
    header: { layout: 'extended', background: 'none', align: 'left', hide_title: false },
    navigation: 'vertical',
    footer: { enabled: true, layout: 'simple', name: '', background: 'none', groups: [] },
  };
  assert.equal(
    describeLookChange(before, after),
    'Theme: ShareCraft to Plum; Logo added; Header: compact to extended; Navigation: horizontal to vertical; Footer turned on',
  );
  // Changed and removed images, and a footer turned off.
  const swapped = { ...structuredClone(after), logo: ref(SHA_B, DOC_B, 'New.png') };
  assert.equal(describeLookChange(after, swapped), 'Logo changed');
  assert.equal(describeLookChange(swapped, { ...structuredClone(swapped), logo: null }), 'Logo removed');
  assert.equal(describeLookChange(after, { ...structuredClone(after), footer: { ...after.footer, enabled: false } }), 'Footer turned off');
  // Footer detail only counts while the footer is on; alignment and hidden title are named.
  assert.equal(
    describeLookChange(after, { ...structuredClone(after), footer: { ...after.footer, layout: 'extended', name: 'Sales' } }),
    'Footer: layout simple to extended, name changed',
  );
  assert.equal(
    describeLookChange(after, { ...structuredClone(after), header: { ...after.header, align: 'centre', background: 'strong' } }),
    'Header: background none to strong, alignment left to centre',
  );
});

test('describe_look_change_treats_no_look_as_the_defaults', () => {
  assert.equal(describeLookChange(null, STORED), 'Logo added');
  assert.equal(describeLookChange(null, { ...structuredClone(DEFAULT_LOOK), navigation: 'vertical' }), 'Navigation: horizontal to vertical');
  assert.equal(describeLookChange(STORED, null), 'Logo removed');
  assert.equal(describeLookChange(null, null), 'No visible change');
  assert.equal(describeLookChange(STORED, structuredClone(STORED)), 'No visible change');
});

test('history_entry_before_previews_and_restores', async () => {
  const { calls, send } = sender();
  const session = lookSession({ siteId: SITE_ID, settings: settingsOf(STORED), themes: THEMES, brandImages: [], send });
  const entry = { event_id: 4, look: STORED, before: null };
  session.preview(entry, 'before');
  assert.equal(session.draft.logo, null, 'no look before is the defaults');
  assert.equal(session.draft.from, 'builtin:sharecraft');
  assert.equal((await session.restore(entry, 'before')).ok, true);
  assert.deepEqual(calls[0], { method: 'POST', path: `/sites/${SITE_ID}/look/rollback`, body: { event_id: 4, which: 'before' } });
  // The default is the resulting look: no `which` is sent.
  await session.restore(entry);
  assert.deepEqual(calls[1].body, { event_id: 4 });
  // A refused rollback of a before loads the before, not the after, as the draft.
  const gone = lookSession({
    siteId: SITE_ID,
    settings: settingsOf(STORED),
    send: async () => ({ ok: false, status: 409, error: 'image_gone', data: { slots: ['logo'] } }),
  });
  const result = await gone.restore({ event_id: 5, look: STORED, before: { ...structuredClone(STORED), navigation: 'vertical' } }, 'before');
  assert.deepEqual(result.gone, ['logo']);
  assert.equal(gone.draft.navigation, 'vertical');
  assert.equal(gone.draft.logo, null);
});

// ---- Plan 4B: fonts ----

const ROLES = ['title', 'heading', 'body', 'interactive'];

test('font_tokens_only_generated_values', () => {
  // The stacks are fixed strings, the same ones the table holds.
  assert.equal(SYSTEM, vectors.font_stacks.system);
  assert.equal(SYSTEM_SERIF, vectors.font_stacks.system_serif);
  assert.deepEqual([...FONT_VARIABLES], ['--font-title', '--font-heading', '--font-body', '--font-interactive']);

  const { sha256, family } = vectors.family_names[0];
  const tokens = fontTokens({
    title: { family, url: `/api/brand/fonts/${sha256}` },
    heading: { family: 'system-serif' },
    body: { family: 'system' },
    interactive: { family: vectors.family_names[1].family },
  });
  assert.deepEqual(Object.keys(tokens), [...FONT_VARIABLES]);
  assert.equal(tokens['--font-title'], `${family}, ${SYSTEM}`);
  assert.equal(tokens['--font-heading'], SYSTEM_SERIF);
  assert.equal(tokens['--font-body'], SYSTEM);
  assert.equal(tokens['--font-interactive'], `${vectors.family_names[1].family}, ${SYSTEM}`);

  // Whatever the server sends, every value is built from a fixed stack and a generated name.
  const allowed = /^(sc-[0-9a-f]{12}, )?(ui-sans-serif, system-ui, 'Segoe UI', sans-serif|ui-serif, Georgia, 'Times New Roman', serif)$/;
  const hostile = [
    '"; } body { display:none',
    'Comic Sans MS',
    'sc-ABCDEF123456',
    'sc-abcdef12345',
    'sc-abcdef1234567',
    'sc-abcdef123456; color:red',
    'sc-abcdef123456, evil',
    'url(https://evil.example/x)',
    '</style><script>1</script>',
    '',
    7,
    null,
    { toString: () => 'sc-abcdef123456' },
  ];
  for (const bad of hostile) {
    const out = fontTokens({ title: { family: bad }, heading: { family: bad }, body: { family: bad }, interactive: { family: bad } });
    for (const name of FONT_VARIABLES) {
      assert.match(out[name], allowed, `${String(bad)}`);
      assert.equal(out[name], SYSTEM, `${String(bad)} is not generated`);
    }
  }
  for (const value of Object.values(tokens)) assert.match(value, allowed);
});

test('font_tokens_fall_back_to_system', () => {
  const system = Object.fromEntries(FONT_VARIABLES.map((name) => [name, SYSTEM]));
  for (const missing of [undefined, null, {}, [], 'x', 5, { title: null }, { title: 'system' }]) {
    assert.deepEqual(fontTokens(missing), system, JSON.stringify(missing));
  }
  // One role missing falls back alone.
  const { family } = vectors.family_names[0];
  const partial = fontTokens({ title: { family }, body: { family: 'system' } });
  assert.equal(partial['--font-title'], `${family}, ${SYSTEM}`);
  assert.equal(partial['--font-heading'], SYSTEM);
  assert.equal(partial['--font-body'], SYSTEM);
  assert.equal(partial['--font-interactive'], SYSTEM);
  assert.deepEqual(Object.keys(partial), ROLES.map((r) => `--font-${r}`));
});

// ---- Plan 4B pages: specimens, the draft's fonts, icon and menu, and copying a look ----

import { applyFonts, fontAsPackage, ownLook, packageFonts } from './look.js';

/** A stand-in for `FontFace` and `document.fonts` that records every face asked for. */
function fontEnv() {
  const faces = [];
  class Face {
    constructor(family, source, options) {
      faces.push({ family, source, options });
    }
    load() {
      return Promise.resolve(this);
    }
  }
  return { faces, env: { FontFace: Face, fonts: { add() {}, delete() {} } } };
}

test('specimen_uses_generated_family', async () => {
  const sha = 'd'.repeat(64);
  const family = `sc-${'d'.repeat(12)}`;
  const url = `/api/brand/fonts/${sha}`;
  // A package as the brand centre lists it: its fonts with generated families and addresses.
  const listed = {
    name: 'Pair',
    display: { name: 'Brand Display.woff2', family, url },
    content: null,
    roles: { title: 'display', heading: 'display', body: 'system', interactive: 'system-serif' },
  };
  const fonts = packageFonts(listed);
  assert.deepEqual(fonts, {
    title: { family, url },
    heading: { family, url },
    body: { family: 'system' },
    interactive: { family: 'system-serif' },
  });
  // A look's own copy names its fonts by content hash: the same family and address.
  const stored = { name: 'Pair', display: { sha256: sha, name: 'Brand Display.woff2' }, roles: listed.roles };
  assert.deepEqual(packageFonts(stored), fonts);
  // A role naming a font the package lacks, and no package at all, are the system fonts.
  assert.deepEqual(packageFonts({ roles: { title: 'content', heading: 'system', body: 'system', interactive: 'system' } }).title, { family: 'system' });
  assert.deepEqual(packageFonts(null).body, { family: 'system' });
  // The specimen is drawn with applyFonts: the generated family before the system stack, loaded
  // from the brand font address through FontFace.
  const { faces, env } = fontEnv();
  const specimen = rootElement();
  await applyFonts(specimen, fonts, env);
  assert.equal(specimen.set['--font-title'], `${family}, ${SYSTEM}`);
  assert.equal(specimen.set['--font-body'], SYSTEM);
  assert.equal(specimen.set['--font-interactive'], SYSTEM_SERIF);
  assert.deepEqual(faces.map((f) => [f.family, f.source]), [[family, `url(${url})`]]);
  // One brand-centre font shown alone sets every role in it.
  const alone = fontAsPackage({ id: 'f1', name: 'Brand Display.woff2', family, url });
  assert.deepEqual(Object.values(packageFonts(alone)), [1, 2, 3, 4].map(() => ({ family, url })));
  // A name or family that is user text never reaches the page, and nothing is loaded for it.
  const hostile = '"; } body { display:none';
  const evil = { name: hostile, display: { name: hostile, family: hostile, url: 'https://evil.test/x.woff2' }, roles: listed.roles };
  const bad = fontEnv();
  const target = rootElement();
  await applyFonts(target, packageFonts(evil), bad.env);
  assert.equal(target.set['--font-title'], SYSTEM);
  assert.equal(bad.faces.length, 0);
  const wrongUrl = fontAsPackage({ name: 'x', family, url: `/api/brand/images/${sha}` });
  assert.deepEqual(packageFonts(wrongUrl).title, { family: 'system' });
  for (const value of Object.values(target.set)) assert.doesNotMatch(value, /display:none|evil/);
});

test('the draft keeps fonts, the tab icon and the menu style, and sends them', () => {
  const sha = 'e'.repeat(64);
  const look = {
    ...structuredClone(STORED),
    fonts: { from: 'org:p1', package: { id: 'p1', name: 'Pair', display: { sha256: sha, name: 'D.woff2' }, roles: { title: 'display', heading: 'display', body: 'system', interactive: 'system' } } },
    icon: ref(SHA_C, DOC_A, 'Icon.png'),
    menu: 'mega',
  };
  const draft = draftFromLook(look);
  assert.equal(draft.fonts.from, 'org:p1');
  assert.equal(draft.fonts.keep, true);
  assert.equal(draft.icon.sha256, SHA_C);
  assert.equal(draft.menu, 'mega');
  const input = lookInput(draft, { themes: THEMES });
  assert.deepEqual(input.fonts, { keep: true }, 'the copy the look holds is kept');
  assert.deepEqual(input.icon, { keep: SHA_C });
  assert.equal(input.menu, 'mega');
  // The preview shows the fonts as the effective look would.
  assert.equal(previewEffective(draft, 'Sales').fonts.title.family, `sc-${'e'.repeat(12)}`);
  assert.equal(previewEffective(draft, 'Sales').navigation_menu, 'mega');
  // A package chosen is sent by its source; the system fonts and the dropdown are left out.
  draft.fonts = { from: 'builtin:classic', name: 'Classic', keep: false };
  assert.deepEqual(lookInput(draft, { themes: THEMES }).fonts, { from: 'builtin:classic' });
  draft.fonts = null;
  draft.menu = 'cascading';
  draft.icon = null;
  const plain = lookInput(draft, { themes: THEMES });
  assert.equal('fonts' in plain, false);
  assert.equal('menu' in plain, false);
  assert.equal('icon' in plain, false);
  // A mega menu is for navigation across the top only.
  draft.menu = 'mega';
  draft.navigation = 'vertical';
  assert.equal('menu' in lookInput(draft, { themes: THEMES }), false);
  // A look without them has none.
  const none = draftFromLook(STORED);
  assert.equal(none.fonts, null);
  assert.equal(none.icon, null);
  assert.equal(none.menu, 'cascading');
  assert.match(describeLookChange(STORED, look), /Fonts: System to Pair/);
  assert.match(describeLookChange(STORED, look), /tab icon added/i);
  assert.match(describeLookChange(STORED, look), /menu style dropdown to mega/i);
});

test('copy_dialog_loads_draft_only', async () => {
  const SOURCE = '22222222-2222-4222-8222-222222222222';
  const copied = { ...structuredClone(STORED), navigation: 'vertical' };
  const { calls, send } = sender(() => ({ ok: true, status: 200, data: settingsOf(copied) }));
  const session = lookSession({ siteId: SITE_ID, settings: settingsOf(STORED), themes: THEMES, brandImages: [], send });
  const effective = {
    theme: {
      light: { accent: '#7a3e8f', accent_ink: '#7a3e8f', on_accent: '#ffffff', header_bg: '#3b2244', header_ink: '#ffffff' },
      dark: { accent: '#c9a0d9', accent_ink: '#c9a0d9', on_accent: '#141c20', header_bg: '#2a1830', header_ink: '#ffffff' },
    },
    fonts: { title: { family: 'system-serif' }, heading: { family: 'system-serif' }, body: { family: 'system' }, interactive: { family: 'system' } },
    logo_alt: 'Marketing',
    header: { layout: 'standard', background: 'strong', align: 'left', hide_title: false },
    navigation: 'vertical',
    navigation_menu: 'cascading',
    footer: { enabled: false, layout: 'simple', name: '', background: 'none', groups: [] },
    theme_from: 'site',
  };
  session.loadCopy({ id: SOURCE, name: 'Marketing' }, effective, { themes: true, fonts: false });
  assert.equal(calls.length, 0, 'choosing a site to copy from saves nothing');
  assert.deepEqual(session.copy, { from: SOURCE, name: 'Marketing', themes: true, fonts: false });
  assert.equal(session.status().dirty, true);
  assert.equal(session.status().canApply, true);
  // The preview shows the other site's look, under this site's name.
  const shown = session.effective('Sales');
  assert.equal(shown.theme.light.accent, '#7a3e8f');
  assert.equal(shown.navigation, 'vertical');
  assert.equal(shown.logo_alt, 'Sales');
  // Cancel drops it, still without a call.
  session.cancel();
  assert.equal(session.copy, null);
  assert.equal(session.status().dirty, false);
  assert.equal(session.effective('Sales').theme.light.accent, '#3156c9');
  assert.equal(calls.length, 0);
  // Apply copies it: one request, the copy route, with the options chosen.
  session.loadCopy({ id: SOURCE, name: 'Marketing' }, effective, { themes: false, fonts: true });
  const result = await session.apply();
  assert.equal(result.ok, true);
  assert.deepEqual(calls, [{ method: 'POST', path: `/sites/${SITE_ID}/look/copy`, body: { from: SOURCE, themes: false, fonts: true } }]);
  assert.equal(session.copy, null);
  assert.equal(session.status().dirty, false);
  assert.equal(session.draft.navigation, 'vertical', 'the copied look is now the saved one');
  // An archived site never copies.
  const archived = lookSession({ siteId: SITE_ID, settings: settingsOf(STORED), themes: THEMES, brandImages: [], send, readOnly: true });
  archived.loadCopy({ id: SOURCE, name: 'Marketing' }, effective, {});
  assert.equal(archived.status().canApply, false);
  assert.equal((await archived.apply()).ok, false);
  assert.equal(calls.length, 1);
});

test('copy_preview_shows_a_hub_members_own_look', () => {
  const SOURCE = '33333333-3333-4333-8333-333333333333';
  const { calls, send } = sender();
  const session = lookSession({ siteId: SITE_ID, settings: settingsOf(STORED), themes: THEMES, brandImages: [], send });
  const hubTheme = {
    light: { accent: '#7a3e8f', accent_ink: '#7a3e8f', on_accent: '#ffffff', header_bg: '#3b2244', header_ink: '#ffffff' },
    dark: { accent: '#c9a0d9', accent_ink: '#c9a0d9', on_accent: '#141c20', header_bg: '#2a1830', header_ink: '#ffffff' },
  };
  const ownTheme = {
    light: { accent: '#2e6b3a', accent_ink: '#2e6b3a', on_accent: '#ffffff', header_bg: '#1d3b24', header_ink: '#ffffff' },
    dark: { accent: '#8fd19e', accent_ink: '#8fd19e', on_accent: '#141c20', header_bg: '#16301c', header_ink: '#ffffff' },
  };
  const serif = { title: { family: 'system-serif' }, heading: { family: 'system-serif' }, body: { family: 'system' }, interactive: { family: 'system' } };
  const system = { title: { family: 'system' }, heading: { family: 'system' }, body: { family: 'system' }, interactive: { family: 'system' } };
  // A site in a hub: the page shows the hub's theme and fonts, `own` is what a copy writes.
  const effective = {
    theme: hubTheme,
    fonts: serif,
    logo_alt: 'Member',
    header: { layout: 'compact', background: 'none', align: 'left', hide_title: false },
    navigation: 'horizontal',
    navigation_menu: 'cascading',
    footer: { enabled: false, layout: 'simple', name: '', background: 'none', groups: [] },
    theme_from: 'hub',
    own: { theme: ownTheme, fonts: system },
  };
  session.loadCopy({ id: SOURCE, name: 'Member' }, effective, {});
  const shown = session.effective('Sales');
  assert.deepEqual(shown.theme, ownTheme, 'the copy preview shows the source site’s own theme');
  assert.deepEqual(shown.fonts, system, 'and its own fonts');
  assert.equal(shown.theme_from, 'site');
  assert.equal('own' in shown, false);
  assert.equal(calls.length, 0);
  // Without `own` (a site not in a hub) the effective look is the site's own already.
  assert.deepEqual(ownLook({ theme: hubTheme, fonts: serif }), { theme: hubTheme, fonts: serif });
  assert.equal(ownLook(null), null);
});

test('header_frame_in_draft_history_and_rollback', async () => {
  const { describeLookChange, draftFromLook, withoutSlots } = await import('./look.js');
  const band = { layout: 'extended', background: 'image', image: { sha256: 'a'.repeat(64), name: 'Band.png' }, align: 'left', hide_title: false };
  const before = { ...structuredClone(STORED), header: band };
  const after = { ...structuredClone(STORED), header: { ...band, frame: { x: 200, y: 800, zoom: 130 } } };
  assert.equal(draftFromLook(before).header.frame, null);
  assert.deepEqual(draftFromLook(after).header.frame, { x: 200, y: 800, zoom: 130 });
  assert.match(describeLookChange(before, after), /image moved/);
  // The middle at its smallest is the same as no framing.
  const middle = { ...structuredClone(STORED), header: { ...band, frame: { x: 500, y: 500, zoom: 100 } } };
  assert.doesNotMatch(describeLookChange(before, middle), /image moved/);
  // Without its image the framing goes too.
  assert.equal(withoutSlots(draftFromLook(after), ['header.image']).header.frame, null);
});
// ---- Page: plain or paper ----

import {
  applyPage,
  DARK_PAPER,
  LIGHT_BACKGROUND,
  LIGHT_PAPER,
  luminance,
  PAPER,
} from './look.js';

test('a draft keeps a paper page and nothing else', () => {
  assert.equal(DEFAULT_LOOK.page, 'plain');
  assert.equal(draftFromLook(null).page, 'plain');
  assert.equal(draftFromLook({ ...structuredClone(STORED), page: 'paper' }).page, 'paper');
  assert.equal(draftFromLook({ ...structuredClone(STORED), page: 'linen' }).page, 'plain');
});

test('Apply sends a paper page only, and the preview shows it', () => {
  const paper = { ...draftFromLook(STORED), page: 'paper' };
  assert.equal(lookInput(paper).page, 'paper');
  assert.equal('page' in lookInput(draftFromLook(STORED)), false);
  assert.equal(previewEffective(paper, 'Team').page, 'paper');
  assert.equal(previewEffective(draftFromLook(null), 'Team').page, 'plain');
});

test('the history says when the page changes', () => {
  assert.equal(describeLookChange(null, { ...structuredClone(DEFAULT_LOOK), page: 'paper' }), 'Page: plain to paper');
  assert.equal(describeLookChange({ ...structuredClone(DEFAULT_LOOK), page: 'paper' }, null), 'Page: paper to plain');
});

/** An element that records only its data-page attribute. */
function pageElement() {
  const attrs = new Map();
  return {
    attrs,
    setAttribute(name, value) {
      attrs.set(name, String(value));
    },
    removeAttribute(name) {
      attrs.delete(name);
    },
  };
}

test('applyPage marks a paper page and clears every other', () => {
  const root = pageElement();
  assert.equal(applyPage(root, { page: 'paper' }), true);
  assert.equal(root.attrs.get('data-page'), 'paper');
  for (const effective of [{ page: 'plain' }, null, { page: 'Paper' }, { page: 'paper"x' }, {}]) {
    applyPage(root, { page: 'paper' });
    assert.equal(applyPage(root, effective), false, JSON.stringify(effective));
    assert.equal(root.attrs.has('data-page'), false, JSON.stringify(effective));
  }
  assert.equal(applyPage(null, { page: 'paper' }), false);
});

test('paper colours read, and sit inside the page colours the theme checks cover', () => {
  const lum = (c) => luminance(parseColour(c));
  for (const key of ['bg', 'paper']) {
    const light = lum(PAPER.light[key]);
    assert.ok(light >= lum(LIGHT_BACKGROUND) && light <= lum(LIGHT_PAPER), `light ${key}`);
    assert.ok(lum(PAPER.dark[key]) <= lum(DARK_PAPER), `dark ${key}`);
    for (const mode of ['light', 'dark']) {
      for (const ink of ['text', 'muted']) {
        const ratio = contrast(PAPER[mode][ink], PAPER[mode][key]);
        assert.ok(ratio >= 4.5, `${mode} ${ink} on ${key}: ${ratio}`);
      }
    }
  }
  assert.deepEqual(PAPER, {
    light: { bg: '#f9f7f2', paper: '#fdfcf9', text: '#252b30', muted: '#667079', champagne: '#b29a70' },
    dark: { bg: '#10151b', paper: '#1c232b', text: '#f0ede6', muted: '#b9c4cd', champagne: '#c7ad82' },
  });
  assert.ok(Object.isFrozen(PAPER) && Object.isFrozen(PAPER.light) && Object.isFrozen(PAPER.dark));
});