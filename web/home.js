import { api, h, messageFor, pageFor, requireMe, setupTopbar, showMessage } from './common.js';

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
        site.role
          ? h('small', {}, `Your role: ${site.role}`)
          : h('small', { class: 'mark' }, 'Limited access'),
      ),
    ),
  );
}
