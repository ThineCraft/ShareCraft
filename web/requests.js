// The open requests for access that this person may decide: approve with a role (within their own
// access) or decline with an optional reply. The server decides; refusals are shown as sentences.

import {
  REQUEST_MAX,
  ageText,
  api,
  h,
  messageFor,
  pageFor,
  requestProblem,
  requireMe,
  rolesWithin,
  seg,
  setupTopbar,
  showMessage,
} from './common.js';

const $ = (id) => document.getElementById(id);
const error = $('error');

const me = await requireMe();
setupTopbar(me, error);

const rolesResult = await api('GET', '/roles');
const roles = rolesResult.ok ? rolesResult.data : [];
await refresh();

function note(text, isError) {
  error.classList.toggle('ok', !isError);
  showMessage(error, text);
}

async function refresh() {
  const result = await api('GET', '/requests');
  if (!result.ok) return note(messageFor(result.error), true);
  $('empty').hidden = result.data.length > 0;
  $('requests').replaceChildren(...result.data.map(requestRow));
}

function requestRow(request) {
  const chain = [...request.path, request.node];
  const trail = h(
    'ol',
    { class: 'crumbs-line' },
    ...chain.map((step, at) =>
      h(
        'li',
        at === chain.length - 1 ? { 'aria-current': 'page' } : {},
        step.access ? h('a', { href: pageFor(step.kind, step.id, step.editor) }, step.name) : h('span', {}, step.name),
      ),
    ),
  );
  const message = request.message
    ? h('p', { class: 'quote' }, request.message)
    : h('p', { class: 'meta' }, 'No message.');

  const choices = rolesWithin(roles, request.node.access);
  const pick = h('select', { id: `role-${request.id}` });
  pick.replaceChildren(
    ...choices.map((role) => {
      const option = h('option', { value: role.id }, role.name);
      option.selected = role.id === 'viewer';
      return option;
    }),
  );
  const reply = h('input', {
    type: 'text',
    id: `reply-${request.id}`,
    maxlength: REQUEST_MAX,
    autocomplete: 'off',
  });
  const problem = h('p', { role: 'alert', class: 'msg' });
  const approve = h('button', { type: 'button' }, 'Approve');
  const decline = h('button', { type: 'button', class: 'plain' }, 'Decline');
  const buttons = [approve, decline];
  const decide = async (call) => {
    showMessage(problem, '');
    for (const button of buttons) button.disabled = true;
    const result = await call();
    if (result.ok) {
      note('Done.', false);
      return refresh();
    }
    for (const button of buttons) button.disabled = false;
    showMessage(problem, messageFor(result.error));
    if (result.error === 'already_decided') await refresh();
  };
  approve.addEventListener('click', () => {
    if (!pick.value) return showMessage(problem, 'There is no role you can give here.');
    return decide(() => api('POST', `/requests/${seg(request.id)}/approve`, { role: pick.value }));
  });
  decline.addEventListener('click', () => {
    const bad = requestProblem(reply.value);
    if (bad) return showMessage(problem, messageFor(bad));
    const text = reply.value.trim();
    return decide(() => api('POST', `/requests/${seg(request.id)}/decline`, text ? { reply: text } : {}));
  });

  return h(
    'li',
    { class: 'role' },
    h('p', { class: 'kind' }, `${request.requester.name} asked ${ageText(request.created_at)}`),
    trail,
    message,
    h(
      'div',
      { class: 'role-form' },
      h('div', { class: 'field' }, h('label', { for: pick.id }, 'Role to give'), pick),
      approve,
    ),
    h(
      'div',
      { class: 'role-form' },
      h('div', { class: 'field' }, h('label', { for: reply.id }, 'Reply to the person (optional)'), reply),
      decline,
    ),
    problem,
  );
}
