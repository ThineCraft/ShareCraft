import {
  ageText,
  api,
  h,
  messageFor,
  pageFor,
  requestDecisionText,
  requireMe,
  roleLine,
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
$('new-site').hidden = !me.is_admin;
// People who can decide requests see a count, if there are any open.
$('requests-link').hidden = !(me.open_requests > 0);
$('requests-count').textContent = String(me.open_requests);
$('requests-link').setAttribute('aria-label', `Requests, ${me.open_requests} open`);

$('new-site').addEventListener('submit', async (event) => {
  event.preventDefault();
  showMessage(error, '');
  const result = await api('POST', '/sites', { name: $('site-name').value.trim() });
  if (result.ok) location.assign(pageFor('site', result.data.id));
  else showMessage(error, messageFor(result.error));
});

const result = await api('GET', '/sites');
if (!result.ok) {
  showMessage(error, messageFor(result.error));
} else {
  const sites = result.data;
  $('empty').hidden = sites.length > 0;
  $('sites').replaceChildren(
    ...sites.map((site) =>
      h(
        'li',
        {},
        h('span', { class: 'grow' }, h('a', { href: pageFor('site', site.id) }, site.name)),
        site.access
          ? h('small', {}, roleLine(site.access))
          : h('small', { class: 'mark' }, 'Limited access'),
      ),
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
