import { api, goToSignIn, h, messageFor, requireMe, seg, showMessage } from './common.js';
import { EDITORS, withExtension } from './editor-host.js';

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

// The page that opens a document: text in the text page, files in the frame around an editor.
function documentPage(doc) {
  return doc.editor === 'text' ? `/doc.html#${seg(doc.id)}` : `/file.html?doc=${seg(doc.id)}`;
}

$('new-doc').addEventListener('submit', async (event) => {
  event.preventDefault();
  showMessage(error, '');
  const editor = $('doc-kind').value;
  const name = $('doc-name').value.trim();
  const extension = Object.hasOwn(EDITORS, editor) ? EDITORS[editor].extension : null;
  const body = extension ? { name: withExtension(name, extension), editor } : name ? { name } : {};
  const result = await api('POST', '/docs', body);
  if (result.ok) location.assign(documentPage(result.data));
  else showMessage(error, messageFor(result.error));
});

$('upload-doc').addEventListener('submit', async (event) => {
  event.preventDefault();
  showMessage(error, '');
  const file = $('upload-file').files[0];
  if (!file) return;
  let response;
  try {
    response = await fetch(`/api/docs/upload?name=${encodeURIComponent(file.name)}`, {
      method: 'POST',
      credentials: 'same-origin',
      headers: { 'Content-Type': 'application/octet-stream' },
      body: file,
    });
  } catch {
    showMessage(error, messageFor('network'));
    return;
  }
  const data = await response.json().catch(() => null);
  if (response.ok && data?.id) {
    location.assign(documentPage(data));
    return;
  }
  const code = typeof data?.error === 'string' ? data.error : 'unknown';
  if (response.status === 401 && code === 'not_signed_in') goToSignIn();
  showMessage(error, uploadProblem(response.status, code));
});

function uploadProblem(status, code) {
  switch (status) {
    case 400:
      return 'That file cannot be uploaded. Upload a .xlsx, .docx, .pdf or .solidcraft.json file that is not empty.';
    case 413:
      return 'That file is too large to upload.';
    case 422:
      return 'That file name is not allowed. Rename the file and try again.';
    default:
      return messageFor(code);
  }
}

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
      const link = h('a', { href: documentPage(doc) }, doc.name);
      const editor = Object.hasOwn(EDITORS, doc.editor) ? EDITORS[doc.editor].name : doc.editor;
      const saved = doc.revised_at ? `Last saved ${formatTime(doc.revised_at)}` : 'Not saved yet';
      return h(
        'li',
        {},
        h('span', { class: 'grow' }, link),
        h('small', {}, editor),
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
