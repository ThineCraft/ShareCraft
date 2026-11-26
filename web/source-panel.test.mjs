import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
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
  mayResolve,
  leftBehindText,
  moveStaysInLibrary,
  moveAcrossLibrariesText,
  blockedText,
  validSharePath,
  hiddenLines,
  fetchingFromShare,
  openRefusedText,
  FETCHING_TEXT,
  SHARE_RETRIES,
  SHARE_RETRY_MS,
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

const ALL = { add: true, folder: true, rename: true, move: true, remove: true, copy: true };

test('sourceActions allows add, folder, rename, move, remove and copy in a writable, unblocked S3 library', () => {
  assert.deepEqual(sourceActions('connected', WRITABLE), ALL);
  assert.deepEqual(sourceActions('connected', { ...WRITABLE, state: 'syncing' }), ALL);
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
    headline: 'Read-only until fixed.',
    detail: 'The storage refused a save: the key may not write.',
    tone: 'attention',
  });
  assert.equal(
    sourceSummary({ ...blocked, write_error: null }, NOW).detail,
    'Nothing more is known. Try again from the library settings.',
  );
});

test('the blocked text names the refusal once', () => {
  const blocked = { ...WRITABLE, write_state: 'blocked', write_error: 'The storage refused a change with this access key.' };
  const summary = sourceSummary(blocked, NOW);
  const banner = `${summary.headline} ${summary.detail}`;
  assert.equal(banner, 'Read-only until fixed. The storage refused a change with this access key.');
  assert.equal(blockedText(blocked), banner);
  assert.equal(
    blockedText({ ...blocked, write_error: null }),
    'Read-only until fixed. Nothing more is known. Try again from the library settings.',
  );
  for (const text of [banner, blockedText(blocked)]) assert.equal(text.match(/refused/g).length, 1);
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
  assert.equal(sourceActionUnavailableText(), 'This cannot be done yet in a library stored in S3.');
  assert.equal(sourceFileTooLargeText(512 * 1024 * 1024), 'Files over 512 MiB cannot be saved to this S3 storage.');
  assert.equal(sourceFileTooLargeText(5 * 1024 ** 3), 'Files over 5 GiB cannot be saved to this S3 storage.');
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

test('sourceWriteErrorText knows the structure codes and shows the server message', () => {
  const sayings = {
    source_too_many_objects:
      'This folder holds 1,500 objects in the bucket. ShareCraft renames, moves, copies or deletes at most 1,000 at a time.',
    source_item_saving:
      'This is still being saved to the storage. Try again when saving has finished, or discard the save that failed.',
    source_too_large_to_move:
      'Files over 5 GiB in S3 storage cannot be renamed, moved, copied or deleted from ShareCraft yet.',
    source_changed:
      'This changed in the storage since ShareCraft last looked. ShareCraft is checking it again; try again in a minute.',
    source_copy_incomplete:
      'The copy stopped part way: 2 items were copied and are being saved to the storage; the rest were not copied.',
  };
  for (const [code, message] of Object.entries(sayings)) {
    assert.equal(sourceWriteErrorText(code, { error: code, message }), message, code);
    // Without a server message the page still says something plain.
    assert.notEqual(sourceWriteErrorText(code, {}), '', code);
    assert.notEqual(sourceWriteErrorText(code), '', code);
  }
  assert.equal(sourceWriteErrorText('source_too_many_objects', { count: 1500 }), sayings.source_too_many_objects);
  assert.equal(
    sourceWriteErrorText('source_action_unavailable', { message: 'This cannot be done yet in a library stored in S3.' }),
    'This cannot be done yet in a library stored in S3.',
  );
  const archived = 'This file is in archive storage and must be restored there before it can be opened.';
  assert.equal(sourceWriteErrorText('source_unavailable', { reason: 'archived', message: archived }), archived);
  // Other reasons of source_unavailable are not this module's to word.
  assert.equal(sourceWriteErrorText('source_unavailable', { reason: 'gone', message: 'x' }), '');
});

test('left_behind has its own sentence', () => {
  const text = (n) =>
    `Done. ${n} of the original objects could not be removed from the bucket; they will show up as separate files until someone removes them.`;
  assert.equal(leftBehindText({ left_behind: 3 }), text(3));
  assert.equal(sourceWriteErrorText('left_behind', { left_behind: 2 }), text(2));
  assert.equal(leftBehindText({ left_behind: 0 }), '');
  assert.equal(leftBehindText({}), '');
  assert.equal(leftBehindText(null), '');
});

test('a move stays inside its S3 library, and says why when it would leave', () => {
  assert.equal(moveStaysInLibrary(WRITABLE, 'lib1', [{ id: 's' }, { id: 'lib1' }, { id: 'f' }]), true);
  assert.equal(moveStaysInLibrary(WRITABLE, 'lib1', [{ id: 's' }, { id: 'lib2' }]), false);
  assert.equal(moveStaysInLibrary(WRITABLE, 'lib1', []), false);
  assert.equal(moveStaysInLibrary(null, 'lib1', [{ id: 'lib2' }]), true);
  assert.equal(moveStaysInLibrary({ repository: 'o/r' }, 'lib1', [{ id: 'lib2' }]), true);
  assert.match(moveAcrossLibrariesText(), /between libraries/);
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

test('the new-library note is read with the link that takes the focus', () => {
  // The note is revealed by un-hiding a status region, which a screen reader may not announce,
  // and the focus then moves to the link: the link names the note as its description.
  const html = readFileSync(new URL('./site.html', import.meta.url), 'utf8');
  const link = html.match(/<a [^>]*id="new-library-open"[^>]*>/)?.[0] ?? '';
  assert.match(link, /aria-describedby="new-library-note"/);
  assert.match(html, /<p id="new-library-note"/);
});

const SHARE = {
  kind: 'share',
  mode: 'read_only',
  path: null,
  last_checked_at: '2026-11-17T14:20:00Z',
  last_synced_at: '2026-11-17T14:20:00Z',
  last_error: null,
  listing_now: null,
  hidden: 0,
  hidden_by: {},
};

test('sourceAccess treats a share answer like any connected source', () => {
  assert.equal(sourceAccess({ ok: true, status: 200, data: { ...SHARE, state: 'ok' } }), 'connected');
  assert.equal(sourceAccess({ ok: true, status: 200, data: { kind: 'share' } }), 'unknown');
  assert.deepEqual(sourceActions('connected', { ...SHARE, state: 'ok' }), NONE);
});

test('sourceSummary describes a share that is shown and checked, with the path only for owners', () => {
  assert.deepEqual(sourceSummary({ ...SHARE, state: 'ok' }, NOW), {
    headline: 'Shows the files in a file share.',
    detail:
      'Checked 4 minutes ago. Files open from the share when you ask for them; this library cannot be changed here.',
    tone: 'ok',
  });
  assert.equal(
    sourceSummary({ ...SHARE, state: 'ok', path: '\\\\server\\share\\docs' }, NOW).headline,
    'Shows the files in \\\\server\\share\\docs.',
  );
});

test('sourceSummary words each state of a share', () => {
  assert.deepEqual(sourceSummary({ ...SHARE, state: 'syncing', listing_now: { files_so_far: 12400 } }, NOW), {
    headline: 'Listing the share… 12,400 files so far',
    detail: 'Files shown are from the last update. This library cannot be changed here.',
    tone: 'working',
  });
  assert.equal(sourceSummary({ ...SHARE, state: 'syncing' }, NOW).headline, 'Listing the share…');
  assert.deepEqual(sourceSummary({ ...SHARE, state: 'connecting', last_checked_at: null }, NOW), {
    headline: 'Connecting…',
    detail: 'Files will appear after the first listing. This library cannot be changed here.',
    tone: 'working',
  });
  const unreachable = 'The share could not be reached. Files already opened still open; others will not until it is back.';
  assert.deepEqual(sourceSummary({ ...SHARE, state: 'failed' }, NOW), {
    headline: unreachable,
    detail: 'Files shown are from the last update. This library cannot be changed here.',
    tone: 'attention',
  });
  assert.equal(
    sourceSummary({ ...SHARE, state: 'failed', last_error: 'The share could not be reached, so nothing was changed.' }, NOW)
      .headline,
    unreachable,
  );
  assert.deepEqual(sourceSummary({ ...SHARE, state: 'not_connected' }, NOW), {
    headline: 'This folder is no longer approved, so its files cannot be opened.',
    detail: 'Files shown are from the last update. This library cannot be changed here.',
    tone: 'attention',
  });
  // A listing over its limits says the server's own sentence.
  const limit = 'The folder holds more than 200,000 files, the most this server shows for one library, so nothing was changed.';
  assert.equal(sourceSummary({ ...SHARE, state: 'failed', last_error: limit }, NOW).headline, limit);
});

test('the GitHub and S3 summaries are unchanged by the share wording', () => {
  assert.equal(
    sourceSummary({ ...base, state: 'ok' }, NOW).headline,
    'Kept in step with github.com/owner/repo (branch main).',
  );
  assert.equal(sourceSummary({ ...s3, state: 'ok' }, S3_NOW).headline, 'Shows the files in S3 storage.');
});

test('validSharePath accepts network, drive and absolute paths', () => {
  for (const path of ['\\\\server\\share\\a', '//server/share/a', 'D:\\data\\a', '/mnt/a', '\\\\server\\share', 'D:\\data\\']) {
    assert.deepEqual(validSharePath(path), { ok: true, reason: '' }, path);
  }
});

test('validSharePath refuses what could never be a share folder, with a reason each', () => {
  for (const path of ['', '   ', 'a\\b', '\\\\server', '\\\\?\\C:\\a', '\\\\s\\sh\\..\\x', 'C:\\', '\\\\s\\sh\\a\\\\b', '/a/./b', '\\\\s\\sh\\a:b']) {
    const answer = validSharePath(path);
    assert.equal(answer.ok, false, JSON.stringify(path));
    assert.notEqual(answer.reason, '', JSON.stringify(path));
  }
  assert.equal(validSharePath(undefined).ok, false);
  assert.equal(validSharePath('\\\\server\\share\\' + 'x'.repeat(1100)).ok, false);
});

test('hiddenLines words each kind plainly and leaves zero counts out', () => {
  assert.deepEqual(hiddenLines({ links: 0, system: 0, offline: 0 }), []);
  assert.deepEqual(hiddenLines(null), []);
  assert.deepEqual(hiddenLines(undefined), []);
  assert.deepEqual(
    hiddenLines({ links: 2, submodules: 0, types: 1, size: 3, names: 1, system: 12, offline: 1, long: 4, unreadable: 1, archived: 9 }),
    [
      '2 links to other places, not followed',
      '12 hidden or system files',
      '1 cloud-only file that is not on the share itself',
      '1 file with a name ShareCraft cannot use',
      '1 file of a type this library does not take',
      '3 files that are too large',
      '4 files with paths that are too long',
      '1 folder the server may not read',
    ],
  );
  assert.deepEqual(hiddenLines({ system: 1200 }), ['1,200 hidden or system files']);
});

test('unavailableText gives a share its own words and leaves the bucket words alone', () => {
  assert.equal(unavailableText('gone', 'share'), 'Not on the share');
  assert.equal(unavailableText('denied', 'share'), 'Access refused');
  assert.equal(unavailableText('offline', 'share'), 'Share not reachable');
  assert.equal(unavailableText('changing', 'share'), 'Changing, try again');
  assert.equal(unavailableText('too_large', 'share'), 'Too large to open');
  assert.equal(unavailableText('not_approved', 'share'), 'Folder not approved');
  assert.equal(unavailableText('anything', 'share'), 'Unavailable');
  assert.equal(unavailableText(undefined, 'share'), 'Unavailable');
  assert.equal(unavailableText('gone'), 'Not in the bucket');
  assert.equal(unavailableMarkFor({ unavailable: 'gone' }, 'share'), 'Not on the share');
  assert.equal(unavailableMarkFor({ name: 'a.txt' }, 'share'), '');
});

test('sourceName for a share never shows a path the source does not carry', () => {
  assert.equal(sourceName({ kind: 'share', path: null }), 'a file share');
  assert.equal(sourceName({ kind: 'share' }), 'a file share');
  assert.equal(sourceName({ kind: 'share', path: '' }), 'a file share');
  assert.equal(sourceName({ kind: 'share', path: '\\\\server\\share\\docs' }), '\\\\server\\share\\docs');
});

test('opening a file: which answers mean ask again, and which are words to show', () => {
  assert.equal(SHARE_RETRIES, 5);
  assert.equal(SHARE_RETRY_MS, 2000);
  assert.equal(FETCHING_TEXT, 'Fetching from the share…');
  assert.equal(fetchingFromShare(503, { error: 'busy' }), true);
  assert.equal(fetchingFromShare(423, { error: 'held', scan: 'pending' }), true);
  assert.equal(fetchingFromShare(423, { error: 'held', scan: 'infected' }), false);
  assert.equal(fetchingFromShare(423, null), false);
  assert.equal(fetchingFromShare(503, { error: 'unavailable' }), false);
  assert.equal(fetchingFromShare(200, null), false);
  assert.equal(openRefusedText(409, { error: 'source_unavailable', reason: 'gone', message: 'The file is no longer available on the file share.' }), 'The file is no longer available on the file share.');
  assert.equal(openRefusedText(409, { error: 'source_unavailable', reason: 'gone' }), 'Not on the share.');
  assert.equal(openRefusedText(409, { error: 'source_copy_unavailable', message: 'Download the file instead.' }), 'Download the file instead.');
  assert.equal(openRefusedText(404, { error: 'not_found' }), '');
  assert.equal(openRefusedText(409, { error: 'source_read_only' }), '');
  assert.equal(openRefusedText(409, null), '');
});

test('mayResolve is true only when the server said the person may Retry or Discard', () => {
  assert.equal(mayResolve({ saving: 'failed', may_resolve: true }), true);
  assert.equal(mayResolve({ state: 'failed', may_resolve: true }), true);
  assert.equal(mayResolve({ saving: 'failed', may_resolve: false }), false);
  assert.equal(mayResolve({ saving: 'failed' }), false);
  assert.equal(mayResolve({ saving: 'failed', may_resolve: 'true' }), false);
  assert.equal(mayResolve(null), false);
});
