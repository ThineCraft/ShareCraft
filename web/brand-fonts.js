// The brand centre's fonts, font packages and limits, without any DOM: whether a font can be
// added (a file, a name and the licence confirmation), which fonts each text role may be set in
// (and why the others are off), what a package form sends, and the limits for site owners as
// five switches in words. brand-centre.js and site-look.js draw them; the server decides.

/** The longest name a brand-centre font may have. */
export const FONT_NAME_MAX = 120;

/**
 * Whether the Add a font form can be sent: `chosen` the file picked (`{doc, name}`), `name` as
 * typed, `confirmed` the licence checkbox. `{canAdd, reason, body}`: `reason` says in words
 * what is missing (the licence confirmation is required), `body` is what `POST
 * /api/brand/fonts` takes once nothing is.
 */
export function fontForm({ chosen, name, confirmed }) {
  const refuse = (reason) => ({ canAdd: false, reason, body: null });
  if (!chosen || typeof chosen.doc !== 'string') return refuse('Choose a font file first.');
  const trimmed = String(name ?? '').trim();
  if (!trimmed) return refuse('Give the font a name.');
  if ([...trimmed].length > FONT_NAME_MAX) return refuse(`Give the font a name of at most ${FONT_NAME_MAX} characters.`);
  if (confirmed !== true) return refuse('Confirm that your organisation holds the licence for this font before adding it.');
  return { canAdd: true, reason: '', body: { doc: chosen.doc, name: trimmed, licence_confirmed: true } };
}

/** The text roles of a package in the order the form shows them, with their names in words. */
export const ROLES = Object.freeze([
  ['title', 'Site title'],
  ['heading', 'Headings'],
  ['body', 'Body text'],
  ['interactive', 'Buttons and controls'],
]);

/**
 * The fonts a role may be set in, for a package that has a display font (`has.display`) and a
 * content font (`has.content`): `[{value, label, disabled, reason}]`. The system fonts are
 * always offered; a font the package lacks is offered disabled, saying why.
 */
export function roleChoices(has = {}) {
  return [
    { value: 'system', label: 'System sans-serif', disabled: false, reason: '' },
    { value: 'system-serif', label: 'System serif', disabled: false, reason: '' },
    has.display
      ? { value: 'display', label: 'Display font', disabled: false, reason: '' }
      : { value: 'display', label: 'Display font', disabled: true, reason: 'Choose a display font first.' },
    has.content
      ? { value: 'content', label: 'Content font', disabled: false, reason: '' }
      : { value: 'content', label: 'Content font', disabled: true, reason: 'Choose a content font first.' },
  ];
}

/**
 * What a package form sends (`POST` or `PATCH /api/brand/font-packages`): `form` holds `name`,
 * `display` and `content` (a brand-centre font id, or '' for none) and `roles`. A role naming a
 * font the package lacks is sent as the system font.
 */
export function packageBody(form) {
  const font = (id) => (typeof id === 'string' && id ? { brand_font: id } : null);
  const display = font(form?.display);
  const content = font(form?.content);
  const roles = {};
  for (const [role] of ROLES) {
    const value = form?.roles?.[role];
    const ok = value === 'system' || value === 'system-serif' || (value === 'display' && display) || (value === 'content' && content);
    roles[role] = ok ? value : 'system';
  }
  return { name: String(form?.name ?? '').trim(), display, content, roles };
}

/** The limits on site owners, in the order the brand centre shows them. */
export const POLICY_KEYS = Object.freeze(['hide_builtin_themes', 'custom_colours', 'site_themes', 'site_images', 'site_fonts']);

/** Each limit as a switch: its label and what it means, in words. */
const POLICY_WORDS = Object.freeze({
  hide_builtin_themes: [
    'Hide the built-in themes',
    'Site owners choose only from your organisation’s themes, not the themes that come with ShareCraft.',
  ],
  custom_colours: ['Let site owners choose their own colours', 'When off, site owners cannot type in colours of their own.'],
  site_themes: ['Let site owners make themes for their site', 'When off, site owners cannot save themes that only their site offers.'],
  site_images: [
    'Let site owners use pictures from their site',
    'When off, logos, icons and header pictures must come from the brand images here.',
  ],
  site_fonts: [
    'Let site owners make font packages for their site',
    'When off, site owners choose only from the font packages here.',
  ],
});

/** The defaults: every limit off. */
const POLICY_DEFAULTS = Object.freeze({ hide_builtin_themes: false, custom_colours: true, site_themes: true, site_images: true, site_fonts: true });

/** The five switches for `policy` (`GET /api/brand`'s), each `{key, label, explanation, checked}`. */
export function policyRows(policy) {
  return POLICY_KEYS.map((key) => {
    const [label, explanation] = POLICY_WORDS[key];
    const value = policy?.[key];
    return { key, label, explanation, checked: typeof value === 'boolean' ? value : POLICY_DEFAULTS[key] };
  });
}
