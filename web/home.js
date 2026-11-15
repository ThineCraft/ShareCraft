// The start page: a greeting, one search field over the site cards (with Kind and Hub behind
// Filters), the files this person opened lately in this browser, and New site for those who
// may create sites.

import {
  ageText,
  api,
  directoryFilter,
  formatDate,
  h,
  messageFor,
  pageFor,
  regionalSettings,
  requestDecisionText,
  requireMe,
  organisationChrome,
  setupTopbar,
  showMessage,
  siteMark,
} from './common.js';
import { fileIcon, fileKind, relativeTime } from './files.js';
import { addPaletteActions } from './palette.js';
import { forgetOpened, greeting, hourIn, readRecent, siteCardModel } from './start.js';

const $ = (id) => document.getElementById(id);
const error = $('error');

let me;
try {
  me = await requireMe();
} catch (err) {
  $('status').textContent = err.message;
  throw err;
}
setupTopbar(me, error);
await organisationChrome();
// The greeting follows the organisation's time zone, and moves on as the day does.
const greet = () => {
  $('greeting').textContent = greeting(hourIn(Date.now(), regionalSettings().timeZone), me.name);
};
greet();
// People who can decide requests see a count, if there are any open.
$('requests-link').hidden = !(me.open_requests > 0);
$('requests-count').textContent = String(me.open_requests);
$('requests-link').setAttribute('aria-label', `Requests, ${me.open_requests} open`);

// ---- New site: a button for those who may create sites, opening a small dialog ----

const newSiteDialog = $('new-site-dialog');
$('new-site-button').hidden = !me.is_admin;
$('new-site-button').addEventListener('click', () => {
  $('site-name').value = '';
  showMessage($('new-site-error'), '');
  newSiteDialog.showModal();
  $('site-name').focus();
});
$('new-site-cancel').addEventListener('click', () => newSiteDialog.close());
newSiteDialog.addEventListener('close', () => $('new-site-button').focus());
// The command palette's New site opens this dialog: here directly, from other pages through #new-site.
if (me.is_admin) {
  addPaletteActions(() => [{ key: 'new-site', label: 'New site', keywords: 'create add make site', run: () => $('new-site-button').click() }]);
  if (location.hash === '#new-site') {
    history.replaceState(null, '', location.pathname + location.search);
    $('new-site-button').click();
  }
}

$('new-site').addEventListener('submit', async (event) => {
  event.preventDefault();
  const say = (text) => showMessage($('new-site-error'), text);
  const name = $('site-name').value.trim();
  if (!name) {
    say('Give the site a name.');
    $('site-name').focus();
    return;
  }
  say('');
  $('new-site-submit').disabled = true;
  const result = await api('POST', '/sites', { name, template: $('site-template').value });
  $('new-site-submit').disabled = false;
  if (result.ok) location.assign(pageFor('site', result.data.id));
  else say(messageFor(result.error));
});

// Administrators choose the template of a new site: the built-in ones and the saved ones.
if (me.is_admin) {
  const templates = await api('GET', '/templates');
  if (templates.ok && templates.data.length > 0) {
    $('site-template').replaceChildren(
      ...templates.data.map((template) =>
        h('option', { value: template.id }, template.built_in ? template.name : `${template.name} (saved template)`),
      ),
    );
  }
}

// ---- Recently opened: the files this person opened in this browser (see start.js) ----

/**
 * Draws the strip. No request is made per file: a file that has gone answers its own page with
 * "does not exist" (and is dropped from the strip there); here each has a Remove button.
 */
function showRecent() {
  const files = readRecent(me.id);
  $('recent-empty').hidden = files.length > 0;
  $('recent').hidden = files.length === 0;
  $('recent').replaceChildren(...files.map(recentItem));
}

function recentItem(file) {
  const iso = new Date(file.at).toISOString();
  const remove = h(
    'button',
    { type: 'button', class: 'plain remove', 'aria-label': `Remove ${file.name} from Recently opened`, title: 'Remove' },
    h('span', { 'aria-hidden': 'true' }, '×'),
  );
  remove.addEventListener('click', () => {
    const item = remove.closest('li');
    const index = [...$('recent').children].indexOf(item);
    forgetOpened(me.id, file.id);
    showRecent();
    $('status').textContent = `${file.name} was removed from Recently opened.`;
    // Focus goes to the file that took its place, or the search field when the strip is empty.
    const items = $('recent').children;
    const next = items[Math.min(index, items.length - 1)]?.querySelector('a');
    (next ?? $('q')).focus();
  });
  return h(
    'li',
    { class: 'recent-file' },
    fileIcon(fileKind(file.name, file.editor)),
    h(
      'span',
      { class: 'recent-text' },
      h('a', { href: pageFor('document', file.id, file.editor), title: file.name }, file.name),
      h('small', {}, file.site ? `${file.site} · ` : '', h('time', { datetime: iso, title: formatDate(iso) }, relativeTime(file.at))),
    ),
    remove,
  );
}

showRecent();
// Another tab may open a file meanwhile; the relative times and the greeting move on too.
addEventListener('storage', (event) => {
  if (event.key === null || event.key.startsWith('sharecraft-recent-files')) showRecent();
});
setInterval(() => {
  greet();
  for (const time of $('recent').querySelectorAll('time')) time.textContent = relativeTime(time.dateTime);
}, 60_000);

// ---- the site cards: the server filters by kind and hub, the page by text as you type ----

const params = new URLSearchParams(location.search);
$('q').value = params.get('q') ?? '';
$('kind').value = ['team', 'communication'].includes(params.get('kind')) ? params.get('kind') : '';
let listed = []; // the sites of the current kind and hub, as the server listed them
let asked = 0; // the latest load; answers to older ones are dropped

const hubs = await api('GET', '/hubs');
if (hubs.ok) {
  $('hub').append(...hubs.data.map((hub) => h('option', { value: hub.id }, hub.name)));
  if (hubs.data.some((hub) => hub.id === params.get('hub'))) $('hub').value = params.get('hub');
}
$('hub').closest('.field').hidden = !hubs.ok || hubs.data.length === 0;
// Filters chosen in the address open the disclosure, so they are never hidden from view.
$('filters').open = $('kind').value !== '' || $('hub').value !== '';

$('find').addEventListener('submit', (event) => event.preventDefault());
$('q').addEventListener('input', () => {
  remember();
  show();
});
$('kind').addEventListener('change', () => load());
$('hub').addEventListener('change', () => load());
$('clear-filters').addEventListener('click', () => {
  $('kind').value = '';
  $('hub').value = '';
  $('kind').focus();
  load();
});
await load();

/** Keeps the search in the address (`?q=&kind=&hub=`), so it can be reloaded or shared. */
function remember() {
  const query = new URLSearchParams();
  for (const name of ['q', 'kind', 'hub']) {
    const value = $(name).value.trim();
    if (value) query.set(name, value);
  }
  const text = query.toString();
  history.replaceState(null, '', text ? `?${text}` : location.pathname);
}

async function load() {
  remember();
  const chosen = ($('kind').value ? 1 : 0) + ($('hub').value ? 1 : 0);
  $('filter-count').textContent = chosen ? ` (${chosen})` : '';
  $('clear-filters').hidden = chosen === 0;
  const query = new URLSearchParams();
  if ($('kind').value) query.set('kind', $('kind').value);
  if ($('hub').value) query.set('hub', $('hub').value);
  const text = query.toString();
  const mine = ++asked;
  const result = await api('GET', `/sites${text ? `?${text}` : ''}`);
  if (mine !== asked) return;
  if (!result.ok) {
    showMessage(error, messageFor(result.error));
    return;
  }
  showMessage(error, '');
  listed = result.data;
  show();
}

function show() {
  const searching = $('q').value.trim() !== '';
  const filtering = searching || $('kind').value !== '' || $('hub').value !== '';
  const sites = directoryFilter(listed, $('q').value);
  $('empty').hidden = filtering || sites.length > 0;
  $('count').textContent = filtering
    ? sites.length === 0
      ? 'No site matches. Try fewer words, or clear the filters.'
      : `${sites.length} ${sites.length === 1 ? 'site matches' : 'sites match'}.`
    : '';
  // While searching, the results sit right under the field.
  $('recent-section').hidden = searching;
  $('sites').replaceChildren(...sites.map(siteCard));
}

/**
 * One site as a card: a band in the site's accent with its mark (thumbnail or initials), its name as the card's
 * only link (stretched over the whole card, one tab stop), kind and hub, a Hub or Archived
 * badge, the description (two lines) and the person's role.
 */
function siteCard(site) {
  const card = siteCardModel(site);
  const band = h('span', { class: 'card-band', 'aria-hidden': 'true' });
  if (card.accent) band.style.setProperty('--card-accent', card.accent);
  // The site's mark sits on the band: its thumbnail, or its initials when it has none.
  band.append(siteMark(site));
  const badges = [card.isHub ? h('span', { class: 'mark own' }, 'Hub') : null, card.archived ? h('span', { class: 'mark' }, 'Archived') : null].filter(Boolean);
  return h(
    'li',
    { class: 'site-card' },
    band,
    h(
      'div',
      { class: 'card-body' },
      h('h3', {}, h('a', { href: card.href, class: 'card-link' }, card.name)),
      badges.length ? h('p', { class: 'card-badges' }, ...badges) : null,
      h('p', { class: 'card-kind' }, card.hub ? `${card.kind} · Hub: ${card.hub}` : card.kind),
      card.description ? h('p', { class: 'card-description' }, card.description) : null,
      h('p', { class: 'card-role' }, card.role),
    ),
  );
}

// The person's own requests that have been decided: an open one is never listed.
const mine = await api('GET', '/requests/mine');
if (mine.ok && mine.data.length > 0) {
  $('mine').hidden = false;
  $('mine-list').replaceChildren(...mine.data.map(requestRow));
}

function requestRow(request) {
  const where = [...request.path.map((step) => step.name), request.node?.name].filter((name) => name != null);
  const item = request.node
    ? h('a', { href: pageFor(request.node.kind, request.node.id, request.node.editor) }, request.node.name)
    : 'An item you can no longer see';
  return h(
    'li',
    {},
    h(
      'span',
      { class: 'grow' },
      item,
      request.node && where.length > 1 ? h('br') : null,
      request.node && where.length > 1 ? h('small', {}, where.join(' › ')) : null,
      request.reply ? h('br') : null,
      request.reply ? h('small', {}, `Reply: ${request.reply}`) : null,
    ),
    h('span', { class: request.decision === 'approved' ? 'mark own' : 'mark' }, requestDecisionText(request)),
    h('small', {}, ageText(request.decided_at)),
  );
}
