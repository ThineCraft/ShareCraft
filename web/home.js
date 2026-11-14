import {
  ageText,
  api,
  directoryFilter,
  directoryRow,
  h,
  messageFor,
  pageFor,
  requestDecisionText,
  requireMe,
  organisationChrome,
  setupTopbar,
  showMessage,
} from './common.js';

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
$('new-site').hidden = !me.is_admin;
// People who can decide requests see a count, if there are any open.
$('requests-link').hidden = !(me.open_requests > 0);
$('requests-count').textContent = String(me.open_requests);
$('requests-link').setAttribute('aria-label', `Requests, ${me.open_requests} open`);

$('new-site').addEventListener('submit', async (event) => {
  event.preventDefault();
  showMessage(error, '');
  const result = await api('POST', '/sites', {
    name: $('site-name').value.trim(),
    template: $('site-template').value,
  });
  if (result.ok) location.assign(pageFor('site', result.data.id));
  else showMessage(error, messageFor(result.error));
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

// ---- the site directory: the server filters by kind and hub, the page by text as you type ----

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

$('find').addEventListener('submit', (event) => event.preventDefault());
$('q').addEventListener('input', () => {
  remember();
  show();
});
$('kind').addEventListener('change', () => load());
$('hub').addEventListener('change', () => load());
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
  const filtering = $('q').value.trim() !== '' || $('kind').value !== '' || $('hub').value !== '';
  const sites = directoryFilter(listed, $('q').value);
  $('empty').hidden = filtering || sites.length > 0;
  $('count').textContent = filtering
    ? sites.length === 0
      ? 'No site matches.'
      : `${sites.length} ${sites.length === 1 ? 'site matches' : 'sites match'}.`
    : '';
  $('sites').replaceChildren(...sites.map(directoryRow));
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
