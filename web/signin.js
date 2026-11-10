import { api, messageFor, safeNext, showMessage } from './common.js';

const form = document.getElementById('form');
const email = document.getElementById('email');
const password = document.getElementById('password');
const submit = document.getElementById('submit');
const error = document.getElementById('error');

form.addEventListener('submit', async (event) => {
  event.preventDefault();
  showMessage(error, '');
  submit.disabled = true;
  const result = await api('POST', '/session', {
    email: email.value.trim(),
    password: password.value,
  });
  submit.disabled = false;
  if (result.ok) {
    const next = new URLSearchParams(location.search).get('next');
    location.assign(safeNext(next));
    return;
  }
  password.value = '';
  showMessage(error, messageFor(result.error));
  password.focus();
});
