// Small pictures of a look, without any DOM: the look a template carries (its theme's colours
// and a font sample, its brand-centre fonts found by id), a site's look from its effective look,
// what saving or importing a template left out of its look, and the results of applying a look
// to many sites, in words. The templates list, the New site wizard and the admin Sites page draw
// them (see lookCardElement in look-card-view.js).

import { ownLook, packageFonts, previewPalettes } from './look.js';

/** A card's colours: the primary and secondary in light and dark mode, from a page's palettes. */
function colours(palettes) {
  return {
    light: { primary: palettes.light.accent, secondary: palettes.light.header_bg },
    dark: { primary: palettes.dark.accent, secondary: palettes.dark.header_bg },
  };
}

/**
 * What a card shows for a template's look (`body.look` of its package, a look whose images and
 * fonts are brand-centre references): `{name, light, dark, fontName, fonts, text}`, `fonts` the
 * sample's (see [`packageFonts`]) with each `{brand_font: id}` found among `brandFonts` (`GET
 * /api/brand`'s `fonts`), a reference not found falling back to the system fonts. Null for a
 * template without a look.
 */
export function templateLookCard(look, brandFonts = []) {
  if (!look || typeof look !== 'object' || !look.theme) return null;
  const find = (ref) => (ref && typeof ref.brand_font === 'string' ? ((brandFonts ?? []).find((f) => f.id === ref.brand_font) ?? null) : null);
  const pkg = look.fonts?.package ?? null;
  const fonts = pkg ? packageFonts({ ...pkg, display: find(pkg.display), content: find(pkg.content) }) : packageFonts(null);
  const name = String(look.theme.name ?? '').trim() || 'Untitled';
  const fontName = pkg ? String(pkg.name ?? '').trim() || 'Untitled' : 'System';
  return {
    name,
    ...colours(previewPalettes(look.theme)),
    fontName,
    fonts,
    text: pkg ? `Look: the theme ${name}, with the fonts ${fontName}.` : `Look: the theme ${name}, with the system fonts.`,
  };
}

/**
 * What a card shows for a site's look, from its effective look (`GET /api/sites/{id}/look`),
 * with the site's own theme and fonts when it is in a hub (see `ownLook`):
 * `{name, light, dark, fonts, text}`. Null when the look could not be read.
 */
export function effectiveLookCard(effective, siteName) {
  // A site in a hub shows the hub's theme and fonts; the card shows the site's own, which a copy gets.
  effective = ownLook(effective);
  if (!effective?.theme?.light || !effective?.theme?.dark) return null;
  return {
    name: String(siteName ?? ''),
    ...colours(effective.theme),
    fonts: effective.fonts ?? packageFonts(null),
    text: `The look of ${siteName}.`,
  };
}

/**
 * What a card shows for a brand-centre theme (`{name, primary, ...}` as listed) with a font
 * package (as listed, or null for the system fonts): `{name, light, dark, fontName, fonts, text}`.
 */
export function brandLookCard(theme, pkg = null) {
  if (!theme) return null;
  const name = String(theme.name ?? '').trim() || 'Untitled';
  const fontName = pkg ? String(pkg.name ?? '').trim() || 'Untitled' : 'System';
  const palettes = theme.resolved?.light && theme.resolved?.dark ? theme.resolved : previewPalettes(theme);
  return {
    name,
    ...colours(palettes),
    fontName,
    fonts: packageFonts(pkg),
    text: `The theme ${name}, with the ${pkg ? `fonts ${fontName}` : 'system fonts'}.`,
  };
}

/** What applying a look to the selected sites does, for each source (`site`, `template`, `brand`). */
export const BULK_LOOK_WARNINGS = Object.freeze({
  site: 'Each selected site gets the whole look of that site: its theme, fonts, pictures, header, navigation style and footer. The site’s own themes and font packages are not copied.',
  template: 'Each selected site gets the whole look the template carries. Brand-centre pictures and fonts it names are pinned to each site.',
  brand: 'Each selected site gets this theme and fonts. Its logo, thumbnail, tab icon, header, navigation, menu and footer stay as they are.',
});

/**
 * The `source` of `POST /api/admin/looks/apply` for a brand-centre theme `theme` and font
 * package `fonts` (both as `from`): `{brand: {theme, fonts?}}`, the fonts left out for the
 * system fonts or none.
 */
export function brandBulkSource(theme, fonts) {
  const brand = { theme };
  if (fonts && fonts !== 'builtin:system') brand.fonts = fonts;
  return { brand };
}

/** The slots of a look in words. */
const SLOT_WORDS = Object.freeze({
  logo: 'the logo',
  thumbnail: 'the thumbnail',
  icon: 'the tab icon',
  'header.image': 'the header image',
  'footer.logo': 'the footer logo',
  'fonts.display': 'the display font',
  'fonts.content': 'the content font',
});
const slotWords = (slot) => SLOT_WORDS[slot] ?? 'a picture or font';

/**
 * What saving a template from a site (`dropped`, `[{slot, name}]`) or importing a package
 * (`unresolved`, `[{slot, id}]`) left out of the template's look, in a sentence; '' for nothing.
 */
export function templateLookReport({ dropped = [], unresolved = [] } = {}) {
  const lines = [];
  if (Array.isArray(dropped) && dropped.length) {
    const named = dropped.map((d) => (d?.name ? `${slotWords(d.slot)} (${d.name})` : slotWords(d?.slot))).join(', ');
    lines.push(
      `The template keeps the site’s look, without these pictures from the site’s own libraries (only brand-centre images go into a template): ${named}.`,
    );
  }
  if (Array.isArray(unresolved) && unresolved.length) {
    lines.push(`Its look refers to brand-centre items this organisation does not have, so they were left out: ${unresolved.map((u) => slotWords(u?.slot)).join(', ')}.`);
  }
  return lines.join(' ');
}

/**
 * `ids` in rounds of at most `size` sites (at least one), in order, each site once: a look is
 * applied round by round, so the progress list fills in as it goes.
 */
export function applyChunks(ids, size) {
  const unique = [...new Set(Array.isArray(ids) ? ids : [])];
  const step = Math.max(1, Math.floor(Number(size) || 1));
  const rounds = [];
  for (let at = 0; at < unique.length; at += step) rounds.push(unique.slice(at, at + step));
  return rounds;
}

/** A site's refusal in words, for the progress list. */
const ERROR_WORDS = Object.freeze({
  archived: 'Skipped: the site is archived.',
  not_found: 'Not changed: the site no longer exists.',
  invalid_look: 'Not changed: the look could not be used.',
  invalid_theme: 'Not changed: the theme could not be used.',
  low_contrast: 'Not changed: the theme’s colours do not read well enough.',
  image_gone: 'Not changed: a picture of the look is no longer stored.',
  held: 'Not changed: a picture or font is being checked for malware.',
  not_an_icon: 'Not changed: the tab icon is not a square PNG or SVG.',
  unavailable: 'Not changed: the server could not do it just now. Try again.',
});

/** How many sites, in words. */
const sites = (n, adjective = '') => `${n} ${adjective ? `${adjective} ` : ''}site${n === 1 ? '' : 's'}`;

/**
 * The results of applying a look to many sites (`POST /api/admin/looks/apply`'s `results`,
 * `[{site, ok, error?, unresolved?}]`) with the sites' names (`names`, a Map of id to name):
 * `{applied, skipped, failed, kind, text, rows}`. `text` is the summary toast in words (`kind`
 * `ok`, or `error` when a site could not be changed); `rows` the progress list, each
 * `{site, name, ok, text}`. Archived sites are counted as skipped.
 */
export function bulkApplySummary(results, names = new Map()) {
  const rows = (Array.isArray(results) ? results : []).map((result) => {
    const name = names.get(result?.site) ?? 'A site';
    if (result?.ok) {
      const missing = Array.isArray(result.unresolved) ? result.unresolved.map((u) => slotWords(u?.slot)) : [];
      const text = missing.length ? `Applied, without ${missing.join(', ')}, which the brand centre does not hold.` : 'Applied.';
      return { site: result.site, name, ok: true, text };
    }
    const text = ERROR_WORDS[result?.error] ?? 'Not changed: the server refused it.';
    return { site: result?.site, name, ok: false, text, skipped: result?.error === 'archived' };
  });
  const applied = rows.filter((r) => r.ok).length;
  const skipped = rows.filter((r) => r.skipped).length;
  const failed = rows.length - applied - skipped;
  const parts = [];
  if (applied) parts.push(`The look was applied to ${sites(applied)}.`);
  if (skipped) parts.push(`${sites(skipped, 'archived')} ${skipped === 1 ? 'was' : 'were'} skipped.`);
  if (failed) parts.push(`${sites(failed)} could not be changed.`);
  const text = parts.length ? parts.join(' ').replace(/^./, (c) => c.toUpperCase()) : 'No site was changed.';
  return { applied, skipped, failed, kind: failed ? 'error' : 'ok', text, rows: rows.map(({ skipped: _s, ...row }) => row) };
}
