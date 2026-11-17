import test from 'node:test';
import assert from 'node:assert/strict';
import {
  sourceAccess,
  sourceControlsEnabled,
  relativeTime,
  sourceStateText,
  sourceSummary,
  validRepository,
  webhookInstructions,
} from './source-panel.js';

test('sourceAccess distinguishes a missing source from connected and uncertain responses', () => {
  const connected = { repository: 'owner/repo', branch: 'main', state: 'ok' };
  assert.equal(sourceAccess({ ok: false, status: 404, error: 'not_found' }), 'none');
  assert.equal(sourceAccess({ ok: false, status: 500, error: 'server_error' }), 'unknown');
  assert.equal(sourceAccess({ ok: false, status: 0, error: 'network' }), 'unknown');
  assert.equal(sourceAccess({ ok: true, status: 200, data: connected }), 'connected');
  assert.equal(sourceAccess({ ok: true, status: 200, data: null }), 'unknown');
});

test('source controls stay disabled while source access is unknown', () => {
  assert.equal(sourceControlsEnabled('unknown'), false);
  assert.equal(sourceControlsEnabled('connected'), false);
  assert.equal(sourceControlsEnabled('none'), true);
});

const NOW = Date.parse('2026-11-17T14:24:00Z');
const base = {
  repository: 'owner/repo',
  branch: 'main',
  last_synced_at: '2026-11-17T14:20:00Z',
};

test('sourceSummary describes an up-to-date source with its repository, branch and age', () => {
  assert.deepEqual(sourceSummary({ ...base, state: 'ok' }, NOW), {
    headline: 'Kept in step with github.com/owner/repo (branch main).',
    detail: 'Updated 4 minutes ago. This library cannot be changed here.',
    tone: 'ok',
  });
});

test('sourceSummary gives connecting and syncing sources a working tone', () => {
  assert.deepEqual(sourceSummary({ ...base, state: 'connecting', last_synced_at: null }, NOW), {
    headline: 'Connecting…',
    detail: 'Files will appear after the first update. This library cannot be changed here.',
    tone: 'working',
  });
  assert.deepEqual(sourceSummary({ ...base, state: 'syncing' }, NOW), {
    headline: 'Updating…',
    detail: 'Files shown are from the last update. This library cannot be changed here.',
    tone: 'working',
  });
});

test('sourceSummary explains rate limits, broken connections and failed updates', () => {
  assert.deepEqual(
    sourceSummary(
      {
        ...base,
        state: 'rate_limited',
        last_error: "GitHub's rate limit is reached; the library is synced again after 2026-11-17 14:20:00.0 +00:00:00",
      },
      NOW,
    ),
    {
      headline: 'Waiting for GitHub (rate limit) until 14:20',
      detail: 'Files shown are from the last update. This library cannot be changed here.',
      tone: 'attention',
    },
  );
  assert.deepEqual(sourceSummary({ ...base, state: 'not_connected', last_error: 'the token was refused' }, NOW), {
    headline: 'Not connected: the token was refused.',
    detail: 'Files shown are from the last update. This library cannot be changed here.',
    tone: 'attention',
  });
  assert.deepEqual(sourceSummary({ ...base, state: 'failed', last_error: 'GitHub could not be reached' }, NOW), {
    headline: 'Update failed: GitHub could not be reached.',
    detail: 'Files shown are from the last update. This library cannot be changed here.',
    tone: 'attention',
  });
});

test('sourceStateText uses the visible state labels', () => {
  assert.equal(sourceStateText('ok'), 'Up to date');
  assert.equal(sourceStateText('connecting'), 'Connecting…');
  assert.equal(sourceStateText('syncing'), 'Updating…');
  assert.equal(sourceStateText('rate_limited'), 'Waiting for GitHub (rate limit)');
  assert.equal(sourceStateText('not_connected'), 'Not connected');
  assert.equal(sourceStateText('failed'), 'Update failed');
});

test('relativeTime says seconds, minutes, hours and days ago', () => {
  assert.equal(relativeTime('2026-11-17T14:23:30Z', NOW), '30 seconds ago');
  assert.equal(relativeTime('2026-11-17T14:20:00Z', NOW), '4 minutes ago');
  assert.equal(relativeTime('2026-11-17T12:24:00Z', NOW), '2 hours ago');
  assert.equal(relativeTime('2026-11-14T14:24:00Z', NOW), '3 days ago');
  assert.equal(relativeTime(null, NOW), '');
});

test('validRepository accepts only a GitHub owner and repository pair', () => {
  for (const repository of ['owner/repo', 'a-b/c.d_e']) assert.equal(validRepository(repository), true, repository);
  for (const repository of ['owner', 'owner/repo/extra', '../x', '', 'https://github.com/owner/repo']) {
    assert.equal(validRepository(repository), false, repository);
  }
});

test('webhookInstructions names the JSON payload, secret and push event without leaking a token', () => {
  const address = 'https://share.example/hooks/github/library-id';
  const secret = 'one-time-secret';
  const lines = webhookInstructions({ url: address, token: 'must-not-leak' }, secret);
  assert.deepEqual(lines, [
    `Payload URL: ${address}`,
    'Content type: application/json',
    `Secret: ${secret}`,
    'Events: Just the push event',
  ]);
  assert.equal(lines.join('\n').split(address).length - 1, 1);
  assert.equal(lines.join('\n').split(secret).length - 1, 1);
  assert.equal(lines.join('\n').includes('must-not-leak'), false);
});
