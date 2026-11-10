import { api, h, messageFor, requireMe, seg, showMessage } from './common.js';

const $ = (id) => document.getElementById(id);
const error = $('error');

let me;
try {
  me = await requireMe();
} catch (err) {
  $('status').textContent = err.message;
  throw err;
}
$('who').textContent = `Signed in as ${me.name}`;
$('people-link').hidden = !me.is_admin;

$('sign-out').addEventListener('click', async () => {
  const result = await api('DELETE', '/session');
  if (result.ok) location.assign('/signin.html');
  else showMessage(error, messageFor(result.error));
});

$('new-doc').addEventListener('submit', async (event) => {
  event.preventDefault();
  showMessage(error, '');
  const name = $('doc-name').value.trim();
  const result = await api('POST', '/docs', name ? { name } : {});
  if (result.ok) location.assign(`/doc.html#${seg(result.data.id)}`);
  else showMessage(error, messageFor(result.error));
});

await showDocuments();

async function showDocuments() {
  const result = await api('GET', '/docs');
  if (!result.ok) {
    showMessage(error, messageFor(result.error));
    return;
  }
  const docs = result.data;
  $('empty').hidden = docs.length > 0;
  $('docs').replaceChildren(
    ...docs.map((doc) => {
      const link = h('a', { href: `/doc.html#${seg(doc.id)}` }, doc.name);
      const saved = doc.revised_at ? `Last saved ${formatTime(doc.revised_at)}` : 'Not saved yet';
      return h(
        'li',
        {},
        h('span', { class: 'grow' }, link),
        h('small', {}, `Your role: ${doc.role}`),
        h('small', {}, saved),
      );
    }),
  );
}

function formatTime(text) {
  const date = new Date(text);
  return Number.isNaN(date.getTime()) ? text : date.toLocaleString();
}
