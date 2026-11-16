import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

const page = readFileSync(new URL('./signin.html', import.meta.url), 'utf8');

test('signin_form_never_puts_the_password_in_the_address', () => {
  // A form sent before signin.js loads must not be a GET, which would put the email and
  // password in the address; and its button starts off until the script turns it on.
  const form = page.match(/<form[^>]*id="form"[^>]*>/)[0];
  assert.match(form, /method="post"/);
  assert.match(form, /action="\/api\/session"/);
  assert.match(page, /<button type="submit" id="submit" disabled>/);
});
