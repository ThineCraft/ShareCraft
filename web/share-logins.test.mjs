import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import {
  loginServers,
  serverOfRoot,
  loginRows,
  loginErrorText,
  removeLoginQuestion,
  runsAsLoginLine,
  LOGIN_HELP,
} from './share-logins.js';

const ROOTS = ['\\\\FileHost\\Pics\\2024', '\\\\filehost\\Other', 'D:\\local\\files', '\\\\Second\\share', '/srv/share'];

test('the server is the lower-case host of a UNC root', () => {
  assert.equal(serverOfRoot('\\\\FileHost\\Pics\\2024'), 'filehost');
  assert.equal(serverOfRoot('D:\\local'), null);
  assert.equal(serverOfRoot('/srv/share'), null);
});

test('the server choice lists only the servers of UNC roots, once each', () => {
  assert.deepEqual(loginServers(ROOTS), ['filehost', 'second']);
  assert.deepEqual(loginServers(['D:\\x']), []);
  assert.deepEqual(loginServers(undefined), []);
});

test('rendering zero logins gives no rows', () => {
  assert.deepEqual(loginRows([]), []);
  assert.deepEqual(loginRows(undefined), []);
});

test('rendering two logins gives a row each with server, user name and set time', () => {
  const rows = loginRows(
    [
      { server: 'filehost', username: 'svc-share', set_at: '2026-11-27T10:00:00Z' },
      { server: 'second', username: 'DOM\\bob', set_at: '2026-11-20T09:00:00Z' },
    ],
    (iso) => `at ${iso}`,
  );
  assert.equal(rows.length, 2);
  assert.deepEqual(rows[0], { server: 'filehost', username: 'svc-share', set: 'at 2026-11-27T10:00:00Z', unc: '\\\\filehost' });
  assert.equal(rows[1].username, 'DOM\\bob');
});

test('a row never carries a password', () => {
  const rows = loginRows([{ server: 'a', username: 'u', set_at: 'x', password: 'hunter2' }], (v) => v);
  assert.ok(!JSON.stringify(rows).includes('hunter2'));
});

test('each error code maps to its sentence', () => {
  const codes = ['login_refused', 'login_conflict', 'server_unreachable', 'no_root_on_server', 'logins_windows_only', 'secret_store_unavailable'];
  const seen = new Set();
  for (const code of codes) {
    const text = loginErrorText(code, 'filehost');
    assert.ok(text.length > 10, code);
    assert.ok(!seen.has(text), `${code} has its own sentence`);
    seen.add(text);
  }
  assert.match(loginErrorText('login_refused', 'filehost'), /\\\\filehost/);
  assert.match(loginErrorText('login_conflict', 'filehost'), /\\\\filehost/);
  assert.match(loginErrorText('server_unreachable', 'filehost'), /\\\\filehost/);
  assert.match(loginErrorText('secret_store_unavailable'), /secret store/);
  assert.match(loginErrorText('invalid_request'), /user name/);
  // The server counts the password in bytes, so the sentence does not promise 256 characters.
  assert.doesNotMatch(loginErrorText('invalid_request'), /password of 1 to 256 characters/);
});

test('an unknown code has no login sentence', () => {
  assert.equal(loginErrorText('nonsense', 'x'), '');
});

test('the remove question and the runs-as line use the exact copy', () => {
  assert.equal(removeLoginQuestion('filehost', 'svc'), 'Remove the login for \\\\filehost? Folders on it will be read as svc.');
  assert.equal(runsAsLoginLine('svc'), 'Without a login, ShareCraft reads as svc.');
  assert.equal(
    LOGIN_HELP,
    'ShareCraft signs in to this server with this login, for every approved folder on it. The password is stored in the secret store and never shown again.',
  );
});

test('the page has the password field as new-password and no value is put back', () => {
  const html = readFileSync(new URL('./sharing.html', import.meta.url), 'utf8');
  assert.match(html, /id="logins-password"[^>]*type="password"|type="password"[^>]*id="logins-password"/);
  assert.match(html, /autocomplete="new-password"/);
  const js = readFileSync(new URL('./sharing.js', import.meta.url), 'utf8');
  assert.doesNotMatch(js, /logins-password'\)\.value\s*=\s*[^'\s;]/);
  assert.match(js, /logins-password'\)\.value = ''/);
});
