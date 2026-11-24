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
  unavailableMarkFor,
  validBucket,
  validPrefix,
  sourceActions,
  savingMark,
  discardQuestion,
  writeCountsText,
  modeSwitchText,
  overwriteNote,
  sourceActionUnavailableText,
  sourceFileTooLargeText,
  sourceWriteBlockedText,
  readWriteAdminOnlyText,
  sourceWriteErrorText,
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

test('unavailableMarkFor gives the text mark only for an item the listing marks unavailable', () => {
  assert.equal(unavailableMarkFor({ name: 'a.txt' }), '');
  assert.equal(unavailableMarkFor({ name: 'a.txt', unavailable: null }), '');
  assert.equal(unavailableMarkFor({ name: 'a.txt', unavailable: 'archived' }), 'In archive storage');
  assert.equal(unavailableMarkFor({ name: 'a.txt', unavailable: 'too_large' }), 'Unavailable');
  assert.equal(unavailableMarkFor(null), '');
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

const WRITABLE = { kind: 's3', state: 'ok', mode: 'read_write', write_state: 'ok', last_checked_at: '2026-11-17T14:20:00Z' };
const NONE = { add: false, folder: false, rename: false, move: false, remove: false, copy: false };

test('sourceActions allows adding and folders only in a writable, unblocked S3 library', () => {
  assert.deepEqual(sourceActions('connected', WRITABLE), { ...NONE, add: true, folder: true });
  assert.deepEqual(sourceActions('connected', { ...WRITABLE, state: 'syncing' }), { ...NONE, add: true, folder: true });
});

test('sourceActions refuses everything in read-only, blocked, GitHub, share and unknown sources', () => {
  assert.deepEqual(sourceActions('connected', { ...WRITABLE, mode: 'read_only' }), NONE);
  assert.deepEqual(sourceActions('connected', { ...WRITABLE, write_state: 'blocked' }), NONE);
  assert.deepEqual(sourceActions('connected', { kind: 's3', state: 'ok' }), NONE);
  assert.deepEqual(sourceActions('connected', { repository: 'o/r', branch: 'main', state: 'ok' }), NONE);
  assert.deepEqual(sourceActions('connected', { kind: 'share', mode: 'read_write', write_state: 'ok' }), NONE);
  assert.deepEqual(sourceActions('unknown', WRITABLE), NONE);
  assert.deepEqual(sourceActions('unknown', null), NONE);
  assert.deepEqual(sourceActions('connected', null), NONE);
});

test('sourceActions allows everything in a library with no source', () => {
  assert.deepEqual(sourceActions('none', null), { add: true, folder: true, rename: true, move: true, remove: true, copy: true });
});

test('sourceSummary for a writable library says changes are saved to the bucket', () => {
  assert.deepEqual(sourceSummary(WRITABLE, NOW), {
    headline: 'Stored in S3 storage.',
    detail: 'Checked 4 minutes ago. Changes you make here are saved to the bucket.',
    tone: 'ok',
  });
});

test('sourceSummary for a blocked writable library says read-only until fixed and why', () => {
  const blocked = { ...WRITABLE, write_state: 'blocked', write_error: 'The storage refused a save: the key may not write.' };
  assert.deepEqual(sourceSummary(blocked, NOW), {
    headline: 'Read-only until fixed: the storage refused a change.',
    detail: 'The storage refused a save: the key may not write.',
    tone: 'attention',
  });
  assert.equal(
    sourceSummary({ ...blocked, write_error: null }, NOW).detail,
    'Nothing more is known. Try again from the library settings.',
  );
});

test('a read-only S3 library keeps its read-only wording', () => {
  const only = { kind: 's3', state: 'ok', mode: 'read_only', last_checked_at: '2026-11-17T14:20:00Z' };
  assert.equal(
    sourceSummary(only, NOW).detail,
    'Checked 4 minutes ago. Files open from the bucket when you ask for them; this library cannot be changed here.',
  );
});

test('a writable library that is syncing does not say it cannot be changed', () => {
  assert.equal(sourceSummary({ ...WRITABLE, state: 'syncing' }, NOW).detail, 'Files shown are from the last update.');
});

test('writeCountsText counts files being saved and files that could not be saved', () => {
  assert.equal(writeCountsText({ saving: 3, failed: 0 }), 'Saving 3 files…');
  assert.equal(writeCountsText({ saving: 1, failed: 0 }), 'Saving 1 file…');
  assert.equal(writeCountsText({ saving: 0, failed: 2 }), '2 files could not be saved.');
  assert.equal(writeCountsText({ saving: 0, failed: 1 }), '1 file could not be saved.');
  assert.equal(writeCountsText({ saving: 3, failed: 2 }), 'Saving 3 files… 2 files could not be saved.');
  assert.equal(writeCountsText({ saving: 0, failed: 0 }), '');
  assert.equal(writeCountsText(null), '');
  assert.equal(writeCountsText({}), '');
});

test('savingMark is text for a saving or failed row and empty for any other', () => {
  assert.equal(savingMark({ saving: 'saving' }), 'Saving…');
  assert.equal(savingMark({ saving: 'failed' }), 'Not saved: it could not be written to the bucket.');
  assert.equal(savingMark({ saving: 'failed' }, 'The storage refused it.'), 'Not saved: The storage refused it.');
  assert.equal(savingMark({ name: 'a.txt' }), '');
  assert.equal(savingMark({ saving: null }), '');
  assert.equal(savingMark({ saving: 'odd' }), '');
  assert.equal(savingMark(null), '');
});

test('discardQuestion asks differently for a new file', () => {
  assert.equal(discardQuestion(false), 'Discard this change? The file goes back to what is in the bucket.');
  assert.equal(discardQuestion(true), 'Discard this file? It was never saved to the bucket.');
});

test('the write error texts are plain sentences', () => {
  assert.equal(
    sourceActionUnavailableText(),
    'This change is not available in a library stored in S3 yet. You can add files and folders.',
  );
  assert.equal(
    sourceFileTooLargeText(512 * 1024 * 1024),
    'This file is larger than the 512.0 MB a library stored in S3 takes. Nothing was stored.',
  );
  assert.equal(sourceFileTooLargeText(undefined), 'This file is larger than a library stored in S3 takes. Nothing was stored.');
  assert.equal(
    sourceWriteBlockedText(),
    'This library is read-only until the storage accepts changes again. An owner can try again from the library settings.',
  );
  assert.equal(readWriteAdminOnlyText(), 'Only an administrator can make a library read and write.');
});

test('sourceWriteErrorText maps the new codes and leaves others empty', () => {
  assert.equal(sourceWriteErrorText('source_action_unavailable'), sourceActionUnavailableText());
  assert.equal(sourceWriteErrorText('source_file_too_large', { limit: 1024 * 1024 }), sourceFileTooLargeText(1024 * 1024));
  assert.equal(sourceWriteErrorText('source_write_blocked'), sourceWriteBlockedText());
  assert.equal(sourceWriteErrorText('read_write_admin_only'), readWriteAdminOnlyText());
  assert.equal(
    sourceWriteErrorText('s3_not_writable'),
    'That access key can read the bucket but is not allowed to write to it. Use a key that may write, or connect read-only.',
  );
  assert.equal(
    sourceWriteErrorText('source_writes_pending'),
    'Some changes are still being saved to the bucket. Wait until they are saved or discarded, then try again.',
  );
  assert.equal(sourceWriteErrorText('not_found'), '');
});

test('modeSwitchText offers only the switch the person may make', () => {
  const rw = { kind: 's3', mode: 'read_write', write_state: 'ok' };
  const ro = { kind: 's3', mode: 'read_only' };
  assert.deepEqual(modeSwitchText(ro, true), { to: 'read_write', label: 'Switch to read and write' });
  assert.equal(modeSwitchText(ro, false), null);
  assert.deepEqual(modeSwitchText(rw, true), { to: 'read_only', label: 'Switch to read-only' });
  assert.deepEqual(modeSwitchText(rw, false), { to: 'read_only', label: 'Switch to read-only' });
  assert.deepEqual(modeSwitchText({ ...rw, write_state: 'blocked' }, true), { to: 'read_write', label: 'Try again' });
  // A blocked library is retried by an administrator only; an owner may still switch it back.
  assert.deepEqual(modeSwitchText({ ...rw, write_state: 'blocked' }, false), { to: 'read_only', label: 'Switch to read-only' });
  assert.equal(modeSwitchText({ kind: 'github', state: 'ok' }, true), null);
  assert.equal(modeSwitchText(null, true), null);
});

test('overwriteNote shows for conditional no or unknown and for nothing else', () => {
  assert.equal(overwriteNote('yes'), "This storage refuses a save that would overwrite someone else's change.");
  assert.equal(
    overwriteNote('no'),
    'This storage cannot refuse a conflicting save. A change made in the bucket in the same instant as a save here can be overwritten.',
  );
  assert.equal(overwriteNote('unknown'), overwriteNote('no'));
  assert.equal(overwriteNote(undefined), '');
  assert.equal(overwriteNote(null), '');
});
