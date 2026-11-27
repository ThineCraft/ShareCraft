import test from 'node:test';
import assert from 'node:assert/strict';

import {
  BLEED_DEFAULT,
  bleedAlpha,
  bleedMask,
  bleedOf,
  bleedReach,
  bleedShows,
  colourBlend,
  luminance,
  parseCssColour,
  readableAlpha,
  readableShade,
} from './bleed.js';

const ratio = (a, b) => {
  const [x, y] = [luminance(a), luminance(b)];
  return (Math.max(x, y) + 0.05) / (Math.min(x, y) + 0.05);
};
const over = (bg, top, a) => bg.map((v, i) => v + (top[i] - v) * a);
const wash = (s, tint, t, bg) => colourBlend(bg, s.map((v, i) => v + (tint[i] - v) * t));

// The page colours of style.css.
const LIGHT = { bg: [0xf5, 0xf7, 0xf8], inks: [[0x23, 0x32, 0x39], [0x5d, 0x6d, 0x75], [0x31, 0x56, 0xc9]] };
const DARK = { bg: [0x14, 0x1c, 0x20], inks: [[0xe7, 0xed, 0xf0], [0xaf, 0xbd, 0xc4], [0x9d, 0xb4, 0xff]] };

test('bleed_of_fills_defaults_and_keeps_values_in_range', () => {
  assert.deepEqual(bleedOf(null), BLEED_DEFAULT);
  assert.deepEqual(bleedOf({ distance: 'far', blur: -3, transparency: 140, tint_light: 'red', enabled: 'yes' }), {
    ...BLEED_DEFAULT,
    blur: 0,
    transparency: 100,
  });
  assert.deepEqual(bleedOf({ enabled: false, distance: 1000, blur: 12.4, tint_dark: '#ABCDEF' }), {
    ...BLEED_DEFAULT,
    enabled: false,
    distance: 400,
    blur: 12,
    tint_dark: '#abcdef',
  });
});

test('bleed_shows_only_when_on_reaching_and_not_wholly_transparent', () => {
  assert.equal(bleedShows(undefined), true);
  assert.equal(bleedShows({ enabled: false }), false);
  assert.equal(bleedShows({ distance: 0 }), false);
  assert.equal(bleedShows({ transparency: 100 }), false);
});

test('bleed_reach_stops_at_the_page', () => {
  assert.deepEqual(bleedReach(240, null, null), { top: 240, right: 240, bottom: 240, left: 240 });
  const header = { top: 95, left: 30, right: 1582, bottom: 255 };
  const page = { top: 0, left: 0, right: 1611, bottom: 2000 };
  assert.deepEqual(bleedReach(240, header, page), { top: 95, right: 29, bottom: 240, left: 30 });
  // A phone: the header spans the page.
  assert.deepEqual(bleedReach(240, { top: 60, left: 0, right: 375, bottom: 180 }, { top: 0, left: 0, right: 375, bottom: 300 }), {
    top: 60,
    right: 0,
    bottom: 120,
    left: 0,
  });
});

test('bleed_mask_fades_over_the_distance_from_the_header', () => {
  const [across, down] = bleedMask(200, { top: 50, right: 0, bottom: 200, left: 30 });
  assert.match(across, /^linear-gradient\(to right,/);
  assert.match(down, /^linear-gradient\(to bottom,/);
  // Nothing at the distance, full at the header's edge; a side cut short fades no sooner.
  assert.ok(across.startsWith('linear-gradient(to right,rgb(0 0 0/0) -170px,'));
  assert.ok(across.includes('rgb(0 0 0/1) 30px'));
  assert.ok(across.includes('rgb(0 0 0/1) calc(100% + 0px)'));
  assert.ok(across.endsWith('rgb(0 0 0/0) calc(100% + 200px))'));
  assert.ok(down.endsWith('rgb(0 0 0/0) calc(100% + 0px))'));
  assert.ok(down.includes('rgb(0 0 0/0.25) -50px'));
});

test('readable_alpha_keeps_every_ink_at_aa_on_any_sample', () => {
  const samples = [
    [0, 0, 0],
    [255, 255, 255],
    [200, 30, 80],
    [40, 120, 200],
  ];
  for (const [scheme, tint] of [
    [LIGHT, [255, 255, 255]],
    [DARK, [0, 0, 0]],
    [LIGHT, [200, 30, 80]],
    [DARK, [40, 120, 200]],
  ]) {
    const a = readableAlpha(samples, tint, scheme.bg, scheme.inks);
    assert.ok(a >= 0 && a <= 1);
    for (const s of samples) {
      const under = over(scheme.bg, wash(s, tint, 0.3, scheme.bg), a);
      for (const ink of scheme.inks) assert.ok(ratio(ink, under) >= Math.min(4.5, ratio(ink, scheme.bg)) - 1e-6, `${ink} on ${under}`);
    }
    // One hundredth more would break it, unless it is already at full strength.
    if (a < 1) {
      const more = a + 0.01;
      const broken = samples.some((s) => scheme.inks.some((ink) => ratio(ink, over(scheme.bg, wash(s, tint, 0.3, scheme.bg), more)) < Math.min(4.5, ratio(ink, scheme.bg)) - 1e-9));
      assert.ok(broken, `${a} is not the most`);
    }
  }
});

test('colour_blend_keeps_the_backdrop_brightness_and_the_source_hue', () => {
  const lum = ([r, g, b]) => 0.3 * r + 0.59 * g + 0.11 * b;
  for (const backdrop of [LIGHT.bg, DARK.bg]) {
    for (const source of [[200, 30, 80], [40, 120, 200], [0, 0, 0], [255, 255, 255]]) {
      const out = colourBlend(backdrop, source);
      assert.ok(Math.abs(lum(out) - lum(backdrop)) < 1e-6, ` over `);
      assert.ok(out.every((v) => v >= -1e-9 && v <= 255 + 1e-9));
    }
  }
  // A red stays reddest.
  const red = colourBlend(LIGHT.bg, [200, 30, 80]);
  assert.ok(red[0] > red[1] && red[0] > red[2]);
});

test('readable_alpha_lets_a_colour_glow_show_fully_on_the_page_colours', () => {
  const pictures = [[0, 0, 0], [255, 255, 255], [200, 30, 80], [40, 120, 200], [255, 220, 0]];
  assert.equal(readableAlpha(pictures, [255, 255, 255], LIGHT.bg, LIGHT.inks), 1);
  assert.equal(readableAlpha(pictures, [0, 0, 0], DARK.bg, DARK.inks), 1);
  assert.equal(readableAlpha([], [255, 255, 255], LIGHT.bg, LIGHT.inks), 1);
});

test('bleed_alpha_is_the_settings_capped', () => {
  assert.equal(bleedAlpha({}), 0.7);
  assert.equal(bleedAlpha({ transparency: 65 }), 0.35);
  assert.equal(bleedAlpha({ transparency: 65 }, 0.1), 0.1);
  assert.equal(bleedAlpha({ transparency: 100 }, 1), 0);
});

test('parse_css_colour_reads_computed_colours', () => {
  assert.deepEqual(parseCssColour('rgb(93, 109, 117)'), [93, 109, 117]);
  assert.deepEqual(parseCssColour('rgba(1, 2, 3, 0.5)'), [1, 2, 3]);
  assert.deepEqual(parseCssColour('color(srgb 1 0 0.5)'), [255, 0, 127.5]);
  assert.equal(parseCssColour('red'), null);
});

test('readable_shade_adds_light_and_shade_only_as_far_as_text_reads', () => {
  const pictures = [[0, 0, 0], [255, 255, 255], [200, 30, 80]];
  for (const [scheme, tint] of [[LIGHT, [255, 255, 255]], [DARK, [0, 0, 0]]]) {
    const strength = 0.7;
    const c = readableShade(pictures, tint, scheme.bg, scheme.inks, strength);
    assert.ok(c > 0 && c < 1, String(c));
    for (const s of pictures) {
      const washed = s.map((v, i) => v + (tint[i] - v) * 0.3);
      const glow = colourBlend(scheme.bg, washed);
      const under = over(scheme.bg, over(glow, washed, c), strength);
      for (const ink of scheme.inks) assert.ok(ratio(ink, under) >= Math.min(4.5, ratio(ink, scheme.bg)) - 1e-6);
    }
  }
  // Nothing showing leaves all the room.
  assert.equal(readableShade([[0, 0, 0]], [0, 0, 0], LIGHT.bg, LIGHT.inks, 0), 1);
});