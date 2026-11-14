import test from 'node:test';
import assert from 'node:assert/strict';
import { createHash, randomBytes } from 'node:crypto';
import {
  CHUNK,
  CONFLICT_CHOICES,
  Sha256,
  UploadQueue,
  conflictQuestion,
  contentRange,
  outcomeNote,
  planChunks,
  splitRelative,
} from './upload.js';

const MIB = 1024 * 1024;

test('chunks are 8 MiB and the last one is whatever is left', () => {
  assert.equal(CHUNK, 8 * MIB);
  assert.deepEqual(planChunks(20 * MIB), [
    { start: 0, end: 8 * MIB - 1 },
    { start: 8 * MIB, end: 16 * MIB - 1 },
    { start: 16 * MIB, end: 20 * MIB - 1 },
  ]);
  assert.deepEqual(planChunks(5), [{ start: 0, end: 4 }]);
  assert.deepEqual(planChunks(0), []);
});

test('a resumed upload plans only what the server has not got', () => {
  assert.deepEqual(planChunks(20 * MIB, 8 * MIB), [
    { start: 8 * MIB, end: 16 * MIB - 1 },
    { start: 16 * MIB, end: 20 * MIB - 1 },
  ]);
  // an offset that is not on a chunk boundary (the server keeps partial chunks)
  assert.deepEqual(planChunks(10, 3, 4), [
    { start: 3, end: 6 },
    { start: 7, end: 9 },
  ]);
  assert.deepEqual(planChunks(10, 10), []);
  assert.deepEqual(planChunks(10, 99), []);
});

test('the Content-Range header names the first and last byte included', () => {
  assert.equal(contentRange({ start: 0, end: 4 }, 5), 'bytes 0-4/5');
  assert.equal(
    contentRange({ start: 8 * MIB, end: 10 * MIB - 1 }, 10 * MIB),
    `bytes ${8 * MIB}-${10 * MIB - 1}/${10 * MIB}`,
  );
});

test('the in-browser SHA-256 matches the standard one across block and chunk edges', () => {
  assert.equal(new Sha256().digestHex(), 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855');
  const abc = new Sha256();
  abc.update(new TextEncoder().encode('abc'));
  assert.equal(abc.digestHex(), 'ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad');
  for (const length of [1, 55, 56, 63, 64, 65, 119, 120, 1000, 100_000]) {
    const bytes = randomBytes(length);
    const want = createHash('sha256').update(bytes).digest('hex');
    const whole = new Sha256();
    whole.update(bytes);
    assert.equal(whole.digestHex(), want, `whole ${length}`);
    const pieces = new Sha256();
    for (let at = 0; at < length; at += 37) pieces.update(bytes.subarray(at, at + 37));
    assert.equal(pieces.digestHex(), want, `pieces ${length}`);
  }
});

test('conflict choices are Replace, Keep both and Skip, with the question asked plainly', () => {
  assert.deepEqual(
    CONFLICT_CHOICES.map((choice) => [choice.value, choice.label]),
    [
      ['replace', 'Replace'],
      ['keep_both', 'Keep both'],
      ['skip', 'Skip'],
    ],
  );
  const text = conflictQuestion('Budget.xlsx', 'Budget (2).xlsx');
  assert.match(text, /Budget\.xlsx/);
  assert.match(text, /Budget \(2\)\.xlsx/);
});

test('a path from a picked folder is split into its folders and its file', () => {
  assert.deepEqual(splitRelative('Photos/2026/a.png'), { folders: ['Photos', '2026'], name: 'a.png' });
  assert.deepEqual(splitRelative('a.png'), { folders: [], name: 'a.png' });
  assert.deepEqual(splitRelative(''), { folders: [], name: '' });
  assert.deepEqual(splitRelative('/x//y.txt'), { folders: ['x'], name: 'y.txt' });
});

test('outcomes tell a person about cleaned names and kept copies', () => {
  assert.match(
    outcomeNote({ status: 'done', name: 'a_b.xlsx', renamed: { from: 'a/b.xlsx', to: 'a_b.xlsx' } }),
    /“a\/b\.xlsx” was saved as “a_b\.xlsx”/,
  );
  assert.match(
    outcomeNote({ status: 'done', name: 'R (2).pdf', kept: { from: 'R.pdf', to: 'R (2).pdf' } }),
    /“R\.pdf” already existed/,
  );
  assert.equal(outcomeNote({ status: 'done', name: 'plain.pdf' }), 'Uploaded.');
  assert.equal(outcomeNote({ status: 'skipped', name: 'x.pdf' }), 'Skipped: a file with that name was already there.');
});

// ---- the queue, against a stubbed server ----

const HASH = 'ab'.repeat(32);
const quickHash = async () => HASH;
const noSleep = async () => {};

function fileOf(name, size, path = []) {
  return { name, size, path, slice: (start, end) => ({ start, end, size: end - start }) };
}

function json(status, body) {
  return { ok: status >= 200 && status < 300, status, json: async () => body };
}

/** A server that keeps its own upload session; `options` can break chosen requests. */
function stubServer(options = {}) {
  const log = [];
  const state = { received: 0, size: 0 };
  const failures = { ...options.fail };
  let putCount = 0;
  const fetch = async (url, init = {}) => {
    const method = init.method ?? 'GET';
    log.push({ method, url, headers: init.headers, body: init.body });
    if (url === '/api/uploads' && method === 'POST') {
      const body = JSON.parse(init.body);
      state.size = body.size;
      state.received = 0;
      if (options.start) return options.start(body);
      return json(201, {
        id: 'sess',
        name: body.name,
        size: body.size,
        received: 0,
        renamed: options.renamedAtStart ?? null,
      });
    }
    if (url === '/api/uploads/sess' && method === 'GET') return json(200, { received: state.received, size: state.size });
    if (url === '/api/uploads/sess' && method === 'PUT') {
      putCount += 1;
      const range = /^bytes (\d+)-(\d+)\/(\d+)$/.exec(init.headers['Content-Range']);
      const start = Number(range[1]);
      const end = Number(range[2]);
      if (start !== state.received) return json(409, { error: 'wrong_offset', expected: state.received });
      const failure = failures[putCount];
      if (failure === 'network') throw new TypeError('connection dropped');
      if (failure === 'partial') {
        state.received = start + 3; // the server kept some of it
        return json(400, { error: 'incomplete_chunk', received: state.received });
      }
      state.received = end + 1;
      return json(200, { received: state.received });
    }
    if (url.startsWith('/api/uploads/sess/finish') && method === 'POST') {
      if (options.finish) return options.finish(url, state);
      return json(201, { id: 'doc1', name: 'x.pdf', editor: 'pdf' });
    }
    if (url === '/api/uploads/sess' && method === 'DELETE') return json(204, null);
    if (url.startsWith('/api/sites/') && method === 'GET') return json(200, options.site ?? { used: 900, quota: 1000 });
    return json(404, { error: 'not_found' });
  };
  return { fetch, log, state };
}

function queueFor(server, extra = {}) {
  return new UploadQueue({ fetch: server.fetch, hash: quickHash, sleep: noSleep, parent: 'lib', site: 'site1', ...extra });
}

test('a file is hashed, sent in 8 MiB chunks and finished', async () => {
  const server = stubServer();
  const [outcome] = await queueFor(server).run([fileOf('x.pdf', 20 * MIB)]);
  assert.equal(outcome.status, 'done');
  const start = JSON.parse(server.log[0].body);
  assert.deepEqual(start, { parent: 'lib', name: 'x.pdf', size: 20 * MIB, sha256: HASH });
  const puts = server.log.filter((call) => call.method === 'PUT');
  assert.deepEqual(
    puts.map((call) => call.headers['Content-Range']),
    [
      `bytes 0-${8 * MIB - 1}/${20 * MIB}`,
      `bytes ${8 * MIB}-${16 * MIB - 1}/${20 * MIB}`,
      `bytes ${16 * MIB}-${20 * MIB - 1}/${20 * MIB}`,
    ],
  );
  assert.equal(server.log.at(-1).url, '/api/uploads/sess/finish');
});

test('progress is reported as bytes arrive', async () => {
  const server = stubServer();
  const seen = [];
  const queue = queueFor(server, { onChange: (item) => seen.push([item.status, item.sent]) });
  await queue.run([fileOf('x.pdf', 20 * MIB)]);
  assert.ok(seen.some(([status]) => status === 'hashing'));
  assert.ok(seen.some(([status, sent]) => status === 'uploading' && sent === 8 * MIB));
  assert.deepEqual(seen.at(-1), ['done', 20 * MIB]);
});

test('a dropped connection resumes from the server’s offset, not from the start', async () => {
  const server = stubServer({ fail: { 2: 'network' } });
  const [outcome] = await queueFor(server).run([fileOf('x.pdf', 20 * MIB)]);
  assert.equal(outcome.status, 'done');
  const puts = server.log.filter((call) => call.method === 'PUT').map((call) => call.headers['Content-Range']);
  // chunk 2 failed, the server was asked how much it had (8 MiB), chunk 2 was sent again
  assert.equal(puts[1], puts[2]);
  assert.ok(server.log.some((call) => call.method === 'GET' && call.url === '/api/uploads/sess'));
  assert.equal(server.log.filter((call) => call.method === 'POST' && call.url === '/api/uploads').length, 1);
});

test('a chunk the server only partly kept resumes after the part it kept', async () => {
  const server = stubServer({ fail: { 1: 'partial' } });
  const [outcome] = await queueFor(server).run([fileOf('x.pdf', 10)]);
  assert.equal(outcome.status, 'done');
  const puts = server.log.filter((call) => call.method === 'PUT').map((call) => call.headers['Content-Range']);
  assert.deepEqual(puts, ['bytes 0-9/10', 'bytes 3-9/10']);
});

test('an upload that keeps failing stops and says the connection was lost', async () => {
  const fail = {};
  for (let n = 1; n <= 20; n += 1) fail[n] = 'network';
  const server = stubServer({ fail });
  const [outcome] = await queueFor(server, { attempts: 3 }).run([fileOf('x.pdf', 10)]);
  assert.equal(outcome.status, 'failed');
  assert.match(outcome.message, /Could not reach the server/);
});

test('a name conflict asks once and applies a choice to the rest when asked', async () => {
  const asked = [];
  const server = stubServer({
    finish: (url) =>
      url.includes('on_conflict=')
        ? json(201, { id: 'd', name: 'x (2).pdf', renamed: { from: 'x.pdf', to: 'x (2).pdf' } })
        : json(409, { error: 'name_taken', suggested: 'x (2).pdf' }),
  });
  const queue = queueFor(server, {
    resolve: async (question) => {
      asked.push(question);
      return { choice: 'keep_both', all: true };
    },
  });
  const outcomes = await queue.run([fileOf('x.pdf', 5), fileOf('y.pdf', 5)]);
  assert.equal(asked.length, 1);
  assert.deepEqual(asked[0], { name: 'x.pdf', suggested: 'x (2).pdf' });
  assert.deepEqual(outcomes.map((o) => o.status), ['done', 'done']);
  assert.deepEqual(outcomes[0].kept, { from: 'x.pdf', to: 'x (2).pdf' });
  const finishes = server.log.filter((call) => call.url.startsWith('/api/uploads/sess/finish')).map((call) => call.url);
  // the second file did not ask: its first finish already carried the remembered choice
  assert.equal(finishes.filter((url) => url.endsWith('?on_conflict=keep_both')).length, 2);
});

test('a choice not marked "for the rest" is asked again for the next conflict', async () => {
  let asked = 0;
  const server = stubServer({
    finish: (url) =>
      url.includes('on_conflict=') ? json(200, { skipped: true }) : json(409, { error: 'name_taken', suggested: 'z (2).pdf' }),
  });
  const queue = queueFor(server, {
    resolve: async () => {
      asked += 1;
      return { choice: 'skip', all: false };
    },
  });
  const outcomes = await queue.run([fileOf('x.pdf', 5), fileOf('y.pdf', 5)]);
  assert.equal(asked, 2);
  assert.deepEqual(outcomes.map((o) => o.status), ['skipped', 'skipped']);
});

test('replace is sent as the choice', async () => {
  const server = stubServer({
    finish: (url) =>
      url.includes('on_conflict=replace')
        ? json(200, { revision: 'r1', changed: true })
        : json(409, { error: 'name_taken', suggested: 'x (2).pdf' }),
  });
  const queue = queueFor(server, { resolve: async () => ({ choice: 'replace', all: false }) });
  const [outcome] = await queue.run([fileOf('x.pdf', 5)]);
  assert.equal(outcome.status, 'done');
  assert.ok(server.log.some((call) => call.url === '/api/uploads/sess/finish?on_conflict=replace'));
});

test('a replace while someone is editing is explained and nothing is lost', async () => {
  const server = stubServer({
    finish: (url) =>
      url.includes('replace') ? json(409, { error: 'being_edited' }) : json(409, { error: 'name_taken', suggested: 'x (2).pdf' }),
  });
  const queue = queueFor(server, { resolve: async () => ({ choice: 'replace', all: false }) });
  const [outcome] = await queue.run([fileOf('x.pdf', 5)]);
  assert.equal(outcome.status, 'failed');
  assert.match(outcome.message, /editing/);
  assert.match(outcome.message, /x\.pdf/);
});

test('an upload dropped because someone else saved meanwhile is explained and can be tried again', async () => {
  const server = stubServer({ finish: () => json(409, { current: 'abc123' }) });
  const [outcome] = await queueFor(server).run([fileOf('x.pdf', 5)]);
  assert.equal(outcome.status, 'failed');
  assert.equal(outcome.retry, true);
  assert.match(outcome.message, /someone else saved/i);
  assert.match(outcome.message, /upload it again/i);
});

test('a version upload for an existing file is started with its document and base', async () => {
  const server = stubServer({ finish: () => json(200, { revision: 'r2', changed: true }) });
  const queue = queueFor(server, { parent: undefined });
  const [outcome] = await queue.run([{ ...fileOf('x.pdf', 5), version: { doc: 'doc1', base: 'r1' } }]);
  assert.equal(outcome.status, 'done');
  assert.deepEqual(JSON.parse(server.log[0].body), { doc: 'doc1', base: 'r1', size: 5, sha256: HASH });
});

test('a quota refusal says how full the site is', async () => {
  const server = stubServer({ start: () => json(507, { error: 'quota_exceeded' }) });
  const [outcome] = await queueFor(server).run([fileOf('Big.pdf', 500)]);
  assert.equal(outcome.status, 'failed');
  assert.match(outcome.message, /Big\.pdf/);
  assert.match(outcome.message, /900 bytes of its 1000 bytes/);
});

test('a quota refusal at the finish is explained the same way', async () => {
  const server = stubServer({ finish: () => json(507, { error: 'quota_exceeded' }) });
  const [outcome] = await queueFor(server).run([fileOf('Big.pdf', 500)]);
  assert.match(outcome.message, /900 bytes of its 1000 bytes/);
});

test('a quota refusal for someone who cannot see the numbers still gets a sentence', async () => {
  const server = stubServer({ start: () => json(507, { error: 'quota_exceeded' }), site: { id: 's' } });
  const [outcome] = await queueFor(server).run([fileOf('Big.pdf', 500)]);
  assert.match(outcome.message, /no room/);
});

test('a cleaned name is reported to the person', async () => {
  const server = stubServer({
    renamedAtStart: { from: 'a/b.xlsx', to: 'a_b.xlsx' },
    finish: () => json(201, { id: 'd', name: 'a_b.xlsx', renamed: { from: 'a/b.xlsx', to: 'a_b.xlsx' } }),
  });
  const [outcome] = await queueFor(server).run([fileOf('a/b.xlsx', 5)]);
  assert.deepEqual(outcome.renamed, { from: 'a/b.xlsx', to: 'a_b.xlsx' });
  assert.equal(outcome.name, 'a_b.xlsx');
});

test('a wrong checksum, an empty file and too many open uploads are each explained', async () => {
  let server = stubServer({ finish: () => json(422, { error: 'checksum_mismatch' }) });
  let [outcome] = await queueFor(server).run([fileOf('x.pdf', 5)]);
  assert.match(outcome.message, /changed while it was being sent|did not arrive intact/);
  server = stubServer();
  [outcome] = await queueFor(server).run([fileOf('empty.pdf', 0)]);
  assert.equal(outcome.status, 'failed');
  assert.match(outcome.message, /empty/i);
  assert.equal(server.log.length, 0);
  server = stubServer({ start: () => json(429, { error: 'too_many_uploads' }) });
  [outcome] = await queueFor(server).run([fileOf('x.pdf', 5)]);
  assert.match(outcome.message, /too many/i);
});

test('one failing file does not stop the others', async () => {
  const server = stubServer({
    start: (body) =>
      body.name === 'bad.pdf'
        ? json(400, { error: 'bad_request' })
        : json(201, { id: 'sess', name: body.name, size: body.size, received: 0 }),
  });
  const outcomes = await queueFor(server).run([fileOf('bad.pdf', 5), fileOf('good.pdf', 5)]);
  assert.deepEqual(outcomes.map((o) => o.status), ['failed', 'done']);
});

test('dropped folders are recreated, reusing one that already exists', async () => {
  const calls = [];
  const server = stubServer();
  const original = server.fetch;
  const fetch = async (url, init = {}) => {
    if (url === '/api/nodes/lib/children' && init.method === 'POST') {
      const body = JSON.parse(init.body);
      calls.push(['create', 'lib', body.name]);
      return json(409, { error: 'name_taken' });
    }
    if (url.startsWith('/api/nodes/lib/children?')) {
      calls.push(['list', 'lib']);
      return json(200, { items: [{ id: 'photos-id', kind: 'folder', name: 'Photos' }], next: null });
    }
    if (url === '/api/nodes/photos-id/children' && init.method === 'POST') {
      const body = JSON.parse(init.body);
      calls.push(['create', 'photos-id', body.name]);
      return json(201, { id: 'id-2026', name: body.name });
    }
    return original(url, init);
  };
  const queue = new UploadQueue({ fetch, hash: quickHash, sleep: noSleep, parent: 'lib', site: 's' });
  const [outcome] = await queue.run([fileOf('a.png', 5, ['Photos', '2026'])]);
  assert.equal(outcome.status, 'done');
  assert.deepEqual(calls, [
    ['create', 'lib', 'Photos'],
    ['list', 'lib'],
    ['create', 'photos-id', '2026'],
  ]);
  assert.equal(JSON.parse(server.log[0].body).parent, 'id-2026');
});

test('the same folder is made once for many files', async () => {
  let made = 0;
  const server = stubServer();
  const original = server.fetch;
  const fetch = async (url, init = {}) => {
    if (url === '/api/nodes/lib/children' && init.method === 'POST') {
      made += 1;
      return json(201, { id: 'dir', name: 'D' });
    }
    return original(url, init);
  };
  const queue = new UploadQueue({ fetch, hash: quickHash, sleep: noSleep, parent: 'lib', site: 's' });
  await queue.run([fileOf('a.pdf', 5, ['D']), fileOf('b.pdf', 5, ['D'])]);
  assert.equal(made, 1);
});
