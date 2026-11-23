import test from 'node:test';
import assert from 'node:assert/strict';
import {
  sourceAccess,
  sourceControlsEnabled,
  relativeTime,
  sourceStateText,
  sourceSummary,
  shouldReload,
  validRepository,
  webhookInstructions,
  sourceName,
  unavailableText,
  validBucket,
  validPrefix,
} from './source-panel.js';

test('shouldReload follows a changed commit or a completed update', () => {
  assert.equal(shouldReload({ state: 'ok', last_commit: 'one' }, { state: 'ok', last_commit: 'two' }), true);
  assert.equal(shouldReload({ state: 'connecting', last_commit: null }, { state: 'ok', last_commit: null }), true);
  assert.equal(shouldReload({ state: 'syncing', last_commit: 'one' }, { state: 'ok', last_commit: 'one' }), true);
  assert.equal(shouldReload({ state: 'ok', last_commit: 'one' }, { state: 'ok', last_commit: 'one' }), false);
  assert.equal(shouldReload(null, { state: 'ok', last_commit: 'one' }), false);
});

test('sourceAccess distinguishes a missing source from connected and uncertain responses', () => {
  const connected = { repository: 'owner/repo', branch: 'main', state: 'ok' };
  assert.equal(sourceAccess({ ok: false, status: 404, error: 'not_found' }), 'none');
  assert.equal(sourceAccess({ ok: false, status: 500, error: 'server_error' }), 'unknown');
  assert.equal(sourceAccess({ ok: false, status: 0, error: 'network' }), 'unknown');
  assert.equal(sourceAccess({ ok: true, status: 200, data: connected }), 'connected');
  assert.equal(sourceAccess({ ok: true, status: 200, data: null }), 'unknown');
  assert.equal(sourceAccess({ ok: true, status: 200, data: { kind: 's3', state: 'ok', bucket: null } }), 'connected');
  assert.equal(sourceAccess({ ok: true, status: 200, data: { kind: 's3' } }), 'unknown');
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

const S3_NOW = Date.parse('2026-11-17T14:24:00Z');
const s3 = {
  kind: 's3',
  mode: 'read_only',
  last_checked_at: '2026-11-17T14:20:00Z',
  last_synced_at: '2026-11-17T14:20:00Z',
  paused: null,
  listing_now: null,
  bucket: 'docs',
  prefix: 'team/',
};

test('sourceSummary describes an S3 library that is shown and checked', () => {
  assert.deepEqual(sourceSummary({ ...s3, state: 'ok' }, S3_NOW), {
    headline: 'Shows the files in S3 storage.',
    detail: 'Checked 4 minutes ago. Files open from the bucket when you ask for them; this library cannot be changed here.',
    tone: 'ok',
  });
});

test('sourceSummary shows listing progress for an S3 library being listed', () => {
  assert.deepEqual(sourceSummary({ ...s3, state: 'syncing', listing_now: { files_so_far: 12400 } }, S3_NOW), {
    headline: 'Listing the bucket… 12,400 files so far',
    detail: 'Files shown are from the last update. This library cannot be changed here.',
    tone: 'working',
  });
  assert.equal(sourceSummary({ ...s3, state: 'syncing', listing_now: null }, S3_NOW).headline, 'Listing the bucket…');
});

test('sourceSummary gives the S3 connecting, unreachable, refused and budget states their words', () => {
  assert.deepEqual(sourceSummary({ ...s3, state: 'connecting', last_checked_at: null }, S3_NOW), {
    headline: 'Connecting…',
    detail: 'Files will appear after the first listing. This library cannot be changed here.',
    tone: 'working',
  });
  assert.deepEqual(sourceSummary({ ...s3, state: 'failed' }, S3_NOW), {
    headline: 'The storage could not be reached. Files already opened still open.',
    detail: 'Files shown are from the last update. This library cannot be changed here.',
    tone: 'attention',
  });
  assert.deepEqual(sourceSummary({ ...s3, state: 'not_connected' }, S3_NOW), {
    headline: 'The storage refused the access key.',
    detail: 'Files shown are from the last update. This library cannot be changed here.',
    tone: 'attention',
  });
  assert.deepEqual(sourceSummary({ ...s3, state: 'ok', paused: 'list' }, S3_NOW), {
    headline: 'This library has used its request budget for the month. Files opened before still open.',
    detail: 'Files shown are from the last update. This library cannot be changed here.',
    tone: 'attention',
  });
});

test('sourceName shows the bucket and folder for owners and never invents a bucket', () => {
  assert.equal(sourceName({ kind: 's3', bucket: 'docs', prefix: 'team/' }), 'docs/team');
  assert.equal(sourceName({ kind: 's3', bucket: 'docs', prefix: '' }), 'docs');
  assert.equal(sourceName({ kind: 's3', bucket: null, prefix: null }), 'S3 storage');
  assert.equal(sourceName({ kind: 's3' }), 'S3 storage');
  assert.equal(sourceName({ repository: 'owner/repo', branch: 'main' }), 'github.com/owner/repo');
});

test('unavailableText gives a word for each unavailable reason', () => {
  assert.equal(unavailableText('archived'), 'In archive storage');
  assert.equal(unavailableText('gone'), 'Not in the bucket');
  assert.equal(unavailableText('denied'), 'Access refused');
  assert.equal(unavailableText('budget'), 'Over the request budget');
  assert.equal(unavailableText('anything'), 'Unavailable');
  assert.equal(unavailableText(undefined), 'Unavailable');
});

test('validBucket accepts S3 bucket names and gives a reason for each refusal', () => {
  for (const name of ['abc', 'my-bucket.2026', 'a1b', 'x'.repeat(63)]) {
    assert.deepEqual(validBucket(name), { ok: true, reason: '' }, name);
  }
  const refused = [
    'ab',
    'x'.repeat(64),
    'My-Bucket',
    'bucket_name',
    '-bucket',
    'bucket-',
    'my..bucket',
    '192.168.0.1',
  ];
  for (const name of refused) {
    const answer = validBucket(name);
    assert.equal(answer.ok, false, name);
    assert.notEqual(answer.reason, '', name);
  }
});

test('validPrefix accepts an empty or plain folder and refuses unsafe ones', () => {
  for (const prefix of ['', 'team', 'team/', 'a/b/c/']) assert.deepEqual(validPrefix(prefix), { ok: true, reason: '' }, prefix);
  for (const prefix of ['/team', 'a/../b', 'a/./b', 'x'.repeat(513), 'bad\u0001name']) {
    const answer = validPrefix(prefix);
    assert.equal(answer.ok, false, JSON.stringify(prefix));
    assert.notEqual(answer.reason, '');
  }
});
