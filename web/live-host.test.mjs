import test from 'node:test';
import assert from 'node:assert/strict';
import {
  MAX_SEND_BYTES,
  connectLive,
  liveNotice,
  liveRetry,
  onlyEditor,
  parseLiveEditorMessage,
  peekServerFrame,
  pickBanner,
  warnOnLeave,
} from './live-host.js';

const ORIGIN = 'https://share.test';
const DOC = 'doc-1';
const SHA = 'ab'.repeat(32);
const TICKET = '00000000-0000-0000-0000-0000000000a1';

// ---- stand-ins ----

/** Timers that run only when the test moves the clock. */
function standInClock() {
  let now = 0;
  let next = 1;
  const timers = new Map();
  const add = (fn, ms, every) => {
    const id = next++;
    timers.set(id, { fn, at: now + ms, every });
    return id;
  };
  return {
    setTimeout: (fn, ms) => add(fn, ms, 0),
    clearTimeout: (id) => timers.delete(id),
    setInterval: (fn, ms) => add(fn, ms, ms),
    clearInterval: (id) => timers.delete(id),
    advance(ms) {
      const until = now + ms;
      for (;;) {
        let due = null;
        for (const [id, timer] of timers) {
          if (timer.at <= until && (due === null || timer.at < due[1].at)) due = [id, timer];
        }
        if (!due) break;
        const [id, timer] = due;
        now = timer.at;
        if (timer.every) timer.at += timer.every;
        else timers.delete(id);
        timer.fn();
      }
      now = until;
    },
  };
}

/** A WebSocket class whose instances the test opens, feeds and closes. */
function standInSockets() {
  const made = [];
  class Socket {
    constructor(url) {
      this.url = url;
      this.sent = [];
      this.readyState = 0;
      made.push(this);
    }
    send(text) {
      if (this.readyState !== 1) throw new Error('not open');
      this.sent.push(text);
    }
    close() {
      if (this.readyState === 3) return;
      this.readyState = 3;
      this.onclose?.({});
    }
    open() {
      this.readyState = 1;
      this.onopen?.({});
    }
    receive(text) {
      this.onmessage?.({ data: text });
    }
  }
  Socket.OPEN = 1;
  return { Socket, made };
}

/** The frame page around a stand-in editor: what the page posts, and the editor's messages. */
function standInPage() {
  const listeners = {};
  const docListeners = {};
  const posted = [];
  const on = (map) => ({
    addEventListener: (type, fn) => (map[type] ??= []).push(fn),
    removeEventListener: (type, fn) => {
      map[type] = (map[type] ?? []).filter((other) => other !== fn);
    },
  });
  const contentWindow = {
    postMessage: (message, origin) => {
      assert.equal(origin, ORIGIN, 'posted to the page origin only');
      posted.push(message);
    },
  };
  const win = { location: { origin: ORIGIN, protocol: 'https:', host: 'share.test' }, ...on(listeners) };
  const ownerDocument = { defaultView: win, visibilityState: 'visible', ...on(docListeners) };
  const frame = { contentWindow, ownerDocument };
  return {
    frame,
    posted,
    ownerDocument,
    fromEditor(data, { source = contentWindow, origin = ORIGIN } = {}) {
      for (const fn of listeners.message ?? []) fn({ data, source, origin });
    },
    fire: (type, event = {}) => (listeners[type] ?? []).forEach((fn) => fn(event)),
    fireDocument: (type, event = {}) => (docListeners[type] ?? []).forEach((fn) => fn(event)),
    kinds: () => posted.map((message) => message.type),
  };
}

/** A fetch that answers from `routes` (url prefix → Response or function) and records calls. */
function standInFetch(routes) {
  const calls = [];
  const fetch = async (url, init = {}) => {
    calls.push({ url, init });
    for (const [prefix, answer] of Object.entries(routes)) {
      if (url.startsWith(prefix)) return typeof answer === 'function' ? answer(url, init) : answer.clone();
    }
    return new Response(null, { status: 404 });
  };
  return { fetch, calls };
}

const settle = async () => {
  for (let i = 0; i < 10; i += 1) await new Promise((resolve) => setImmediate(resolve));
};

/** A live connection on stand-ins, its socket opened. */
function live({ routes = {}, handlers = {}, open = true } = {}) {
  const page = standInPage();
  const clock = standInClock();
  const sockets = standInSockets();
  const fetched = standInFetch(routes);
  const seen = { removed: [], stopped: [], reloads: 0, offers: 0, peeks: [] };
  const connection = connectLive(page.frame, { prefix: 'gridcraft', live: 'gridcraft/1' }, DOC, {
    canEdit: true,
    look: (person) => ({ initials: 'AT', colour: '#2f5bd3', name: person.name }),
    removed: (reason) => seen.removed.push(reason),
    stopped: (why) => seen.stopped.push(why),
    reloadEditor: () => (seen.reloads += 1),
    offerReload: () => (seen.offers += 1),
    peek: (message) => seen.peeks.push(message.type),
    ...handlers,
    env: {
      WebSocket: sockets.Socket,
      fetch: fetched.fetch,
      setTimeout: clock.setTimeout,
      clearTimeout: clock.clearTimeout,
      setInterval: clock.setInterval,
      clearInterval: clock.clearInterval,
      random: () => 0,
    },
  });
  if (open) sockets.made.at(-1).open();
  return { page, clock, sockets, fetched, seen, connection, socket: () => sockets.made.at(-1) };
}

// ---- back-off ----

test('live_retry_doubles_to_sixty_seconds_with_spread', () => {
  let state;
  const waits = [];
  for (let n = 0; n < 9; n += 1) {
    state = liveRetry(state, { type: 'closed' }, () => 0);
    waits.push(state.wait);
  }
  assert.deepEqual(waits, [1000, 2000, 4000, 8000, 16000, 32000, 60000, 60000, 60000]);
  assert.equal(state.attempt, 9);
  assert.equal(state.stopped, false);
  // Up to 20% spread, never over the base and never over a minute.
  for (const random of [0, 0.25, 0.5, 0.999]) {
    let spread;
    for (let n = 0; n < 8; n += 1) {
      spread = liveRetry(spread, { type: 'closed' }, () => random);
      const base = Math.min(1000 * 2 ** n, 60000);
      assert.ok(spread.wait <= base && spread.wait >= base * 0.8, `${random} ${n} ${spread.wait}`);
    }
  }
  assert.equal(liveRetry(undefined, { type: 'closed' }, () => 0.5).wait, 900);
});

test('live_retry_stops_after_five_identical_client_errors', () => {
  let state;
  for (let n = 0; n < 4; n += 1) {
    state = liveRetry(state, { type: 'reset', kind: 'desync' }, () => 0);
    // A close between errors is not an error and does not break the run.
    state = liveRetry(state, { type: 'closed' }, () => 0);
  }
  assert.equal(state.stopped, false);
  state = liveRetry(state, { type: 'reset', kind: 'desync' }, () => 0);
  assert.equal(state.stopped, true);
  assert.equal(state.lastError, 'desync');
  // Stopped stays stopped.
  assert.equal(liveRetry(state, { type: 'closed' }, () => 0).stopped, true);

  // A different error starts the count again.
  let mixed;
  for (const kind of ['desync', 'desync', 'desync', 'desync', 'base', 'desync', 'desync', 'desync', 'desync']) {
    mixed = liveRetry(mixed, { type: 'reset', kind }, () => 0);
  }
  assert.equal(mixed.stopped, false);
  assert.equal(mixed.lastError, 'desync');
});

test('live_retry_resets_once_live', () => {
  let state;
  for (let n = 0; n < 4; n += 1) state = liveRetry(state, { type: 'reset', kind: 'base' }, () => 0);
  assert.equal(state.wait, 8000);
  state = liveRetry(state, { type: 'live' }, () => 0);
  assert.deepEqual(state, { wait: 0, attempt: 0, stopped: false, lastError: null, repeats: 0 });
  state = liveRetry(state, { type: 'reset', kind: 'base' }, () => 0);
  assert.equal(state.wait, 1000);
  assert.equal(state.repeats, 1);
});

// ---- messages ----

test('editor_live_messages_with_another_prefix_are_ignored', () => {
  const frame = '{"type":"heartbeat","seen":0}';
  assert.equal(parseLiveEditorMessage('gridcraft', { type: 'wordcraft:live-send', frame }), null);
  assert.equal(parseLiveEditorMessage('gridcraft', { type: 'gridcraftx:live-send', frame }), null);
  assert.equal(parseLiveEditorMessage('gridcraft', { type: 'gridcraft:send', frame }), null);
  assert.equal(parseLiveEditorMessage('gridcraft', { type: 'live-send', frame }), null);
  // The file contract and the page's own kinds are not the editor's live messages.
  assert.equal(parseLiveEditorMessage('gridcraft', { type: 'gridcraft:ready', version: '1' }), null);
  assert.equal(parseLiveEditorMessage('gridcraft', { type: 'gridcraft:live-frame', frame }), null);
  assert.equal(parseLiveEditorMessage('gridcraft', { type: 'gridcraft:live-start', document: DOC }), null);
  assert.deepEqual(parseLiveEditorMessage('gridcraft', { type: 'gridcraft:live-send', frame }), {
    kind: 'send',
    frame,
  });
  assert.deepEqual(parseLiveEditorMessage('gridcraft', { type: 'gridcraft:live-reset', kind: 'overloaded' }), {
    kind: 'reset',
    reset: 'overloaded',
  });
  assert.deepEqual(
    parseLiveEditorMessage('gridcraft', { type: 'gridcraft:live-state', phase: 'live', waiting: 2, canEdit: true }),
    { kind: 'state', phase: 'live', waiting: 2, canEdit: true },
  );
  const notice = { kind: 'read_only', read_only: true };
  assert.deepEqual(parseLiveEditorMessage('gridcraft', { type: 'gridcraft:live-notice', notice }), {
    kind: 'notice',
    notice,
  });
});

test('malformed_live_messages_are_ignored', () => {
  const bytes = new Uint8Array([1, 2]);
  const sessionData = new Uint8Array([3]);
  const checkpoint = { type: 'gridcraft:live-checkpoint', ticket: TICKET, at: 4, base: 'r1', bytes, sessionData, sha256: SHA };
  for (const data of [
    null,
    undefined,
    'gridcraft:live-send',
    7,
    ['gridcraft:live-send'],
    {},
    { type: 3 },
    { type: 'gridcraft:live-send' },
    { type: 'gridcraft:live-send', frame: 7 },
    { type: 'gridcraft:live-send', frame: 'x'.repeat(1024 * 1024 + 1) },
    { type: 'gridcraft:live-reset' },
    { type: 'gridcraft:live-reset', kind: 'x'.repeat(65) },
    { type: 'gridcraft:live-load', revision: 'r1', at: -1 },
    { type: 'gridcraft:live-load', revision: 'r1', at: 1.5 },
    { type: 'gridcraft:live-load', revision: 'r1', at: '3' },
    { type: 'gridcraft:live-load', revision: 7, at: 3 },
    { type: 'gridcraft:live-load', revision: 'r'.repeat(129), at: 3 },
    { ...checkpoint, bytes: undefined },
    { ...checkpoint, bytes: [1, 2] },
    { ...checkpoint, sessionData: 'abc' },
    { ...checkpoint, sha256: 'AB'.repeat(32) },
    { ...checkpoint, sha256: 'ab' },
    { ...checkpoint, ticket: undefined },
    { ...checkpoint, ticket: 'x'.repeat(65) },
    { ...checkpoint, base: 5 },
    { type: 'gridcraft:live-hash', ticket: TICKET, at: 4 },
    { type: 'gridcraft:live-hash', ticket: TICKET, at: -4, sha256: SHA },
    { type: 'gridcraft:live-notice' },
    { type: 'gridcraft:live-notice', notice: { read_only: true } },
    { type: 'gridcraft:live-notice', notice: ['kind'] },
    { type: 'gridcraft:live-state', phase: 'live', waiting: -1, canEdit: true },
    { type: 'gridcraft:live-state', phase: 'live', waiting: 0, canEdit: 'yes' },
    { type: 'gridcraft:live-state', phase: 7, waiting: 0, canEdit: true },
  ]) {
    assert.equal(parseLiveEditorMessage('gridcraft', data), null, JSON.stringify(data)?.slice(0, 80));
  }
  // A good checkpoint carries copies of its bytes.
  const parsed = parseLiveEditorMessage('gridcraft', checkpoint);
  assert.equal(parsed.kind, 'checkpoint');
  assert.deepEqual([...parsed.bytes], [1, 2]);
  assert.deepEqual([...parsed.sessionData], [3]);
  bytes[0] = 9;
  assert.equal(parsed.bytes[0], 1, 'the page keeps its own copy');
  assert.equal(parseLiveEditorMessage('gridcraft', { ...checkpoint, base: null }).base, null);
});

test('page_peeks_only_at_named_server_messages', () => {
  const named = {
    people: '{"type":"people","people":[]}',
    access: '{"type":"access","can_edit":false}',
    removed: '{"type":"removed","reason":"deleted"}',
    expired: '{"type":"expired"}',
    incompatible: '{"type":"incompatible","protocol":1,"format":"gridcraft/1"}',
    build_refused: '{"type":"build_refused","build":"desktop"}',
    cannot_open: '{"type":"cannot_open","seq":4}',
    changes_paused: '{"type":"changes_paused","entries":1,"bytes":2}',
    changes_resumed: '{"type":"changes_resumed"}',
    returning: '{"type":"returning","participant":3}',
    saved: '{"type":"saved","revision":"r1","through":4,"verified":false,"major":false}',
  };
  for (const [type, frame] of Object.entries(named)) {
    assert.equal(peekServerFrame(frame)?.type, type, frame);
  }
  for (const frame of [
    '{"type":"entry","entry":{"seq":1,"kind":"change","payload":"{\\"type\\":\\"removed\\"}"}}',
    '{"type":"presence","participant":2,"payload":"{\\"type\\":\\"removed\\"}"}',
    '{"type":"catchup","base":0,"revision":null,"from":0,"head":0}',
    '{"type":"live_joined","session":"x"}',
    '{"type":"checkpoint_ticket","ticket":"x"}',
    '{"type":"removed",',
    'not json',
    '',
    ' {"type":"removed","reason":"deleted"}',
  ]) {
    assert.equal(peekServerFrame(frame), null, frame);
  }
});

// ---- the connection ----

test('frames_pass_through_unchanged', async () => {
  const { page, socket, sockets, seen, clock } = live();
  assert.deepEqual(page.posted[0], { type: 'gridcraft:live-start', document: DOC, format: 'gridcraft/1', canEdit: true });
  assert.equal(socket().url, 'wss://share.test/ws/docs/doc-1');
  assert.deepEqual(page.posted[1], { type: 'gridcraft:live-open' });
  const join = '{"type":"live_join",  "client":"x"}';
  page.fromEditor({ type: 'gridcraft:live-send', frame: join });
  assert.deepEqual(socket().sent, [join]);
  // Messages from anything but the editor, or from another origin, are not taken.
  page.fromEditor({ type: 'gridcraft:live-send', frame: 'other' }, { source: {} });
  page.fromEditor({ type: 'gridcraft:live-send', frame: 'other' }, { origin: 'https://evil.test' });
  assert.deepEqual(socket().sent, [join]);
  const frames = [
    '{"type":"entry","entry":{"seq":1,"payload":"  odd spacing \\u0001"}}',
    '{"type":"presence","participant":2,"payload":"B2"}',
    '{"type":"removed","reason":"deleted"}',
  ];
  for (const frame of frames) socket().receive(frame);
  const relayed = page.posted.filter((message) => message.type === 'gridcraft:live-frame').map((message) => message.frame);
  assert.deepEqual(relayed, frames);
  // Only the named frame was looked at; removed stops for good.
  assert.deepEqual(seen.removed, ['deleted']);
  clock.advance(120_000);
  assert.equal(sockets.made.length, 1, 'no reconnect after removed');
});

test('a_reset_closes_and_reconnects_after_back_off', () => {
  const { page, socket, sockets, clock, seen } = live();
  page.fromEditor({ type: 'gridcraft:live-reset', kind: 'desync' });
  assert.equal(socket().readyState, 3);
  assert.equal(page.kinds().at(-1), 'gridcraft:live-closed');
  clock.advance(999);
  assert.equal(sockets.made.length, 1);
  clock.advance(1);
  assert.equal(sockets.made.length, 2, 'the page waits one second, then joins again');
  sockets.made[1].open();
  for (let n = 0; n < 4; n += 1) {
    page.fromEditor({ type: 'gridcraft:live-reset', kind: 'desync' });
    clock.advance(120_000);
    socket().open();
  }
  assert.deepEqual(seen.stopped, ['repeated']);
  const made = sockets.made.length;
  clock.advance(600_000);
  assert.equal(sockets.made.length, made, 'stopped after five identical errors');
});

test('base_load_fetches_revision_and_session_data', async () => {
  const file = new Response(new Uint8Array([1, 2, 3]), { status: 200 });
  const sessionData = new Response(new Uint8Array([9]), {
    status: 200,
    headers: { 'x-sharecraft-live-at-ms': '1700', 'x-sharecraft-live-seed': '42' },
  });
  const { page, fetched } = live({
    routes: {
      '/api/docs/doc-1/content?revision=r1': file,
      '/api/docs/doc-1/live-base?at=5': sessionData,
      '/api/docs/doc-1/live-base?at=6': new Response(null, { status: 204 }),
    },
  });
  page.fromEditor({ type: 'gridcraft:live-load', revision: 'r1', at: 5 });
  await settle();
  const base = page.posted.find((message) => message.type === 'gridcraft:live-base');
  assert.equal(base.revision, 'r1');
  assert.equal(base.at, 5);
  assert.deepEqual([...base.bytes], [1, 2, 3]);
  assert.deepEqual([...base.sessionData], [9]);
  assert.deepEqual(base.stamp, { atMs: 1700, seed: 42 });
  assert.ok(fetched.calls.every((call) => call.init.credentials === 'same-origin'));

  // A plain file revision: no session data and no stamp.
  page.posted.length = 0;
  page.fromEditor({ type: 'gridcraft:live-load', revision: 'r1', at: 6 });
  await settle();
  const plain = page.posted.find((message) => message.type === 'gridcraft:live-base');
  assert.deepEqual([...plain.bytes], [1, 2, 3]);
  assert.equal('sessionData' in plain, false);
  assert.equal('stamp' in plain, false);

  // No revision: the empty document, fetched from nowhere.
  page.posted.length = 0;
  const before = fetched.calls.length;
  page.fromEditor({ type: 'gridcraft:live-load', revision: null, at: 0 });
  await settle();
  const empty = page.posted.find((message) => message.type === 'gridcraft:live-base');
  assert.equal(empty.revision, null);
  assert.equal(empty.bytes.length, 0);
  assert.equal(fetched.calls.length, before);
});

test('a_base_that_cannot_be_fetched_resets_the_connection', async () => {
  const { page, socket, sockets, clock } = live({
    routes: { '/api/docs/doc-1/content?revision=r1': new Response(null, { status: 503 }) },
  });
  page.fromEditor({ type: 'gridcraft:live-load', revision: 'r1', at: 5 });
  await settle();
  assert.equal(page.posted.some((message) => message.type === 'gridcraft:live-base'), false);
  assert.equal(socket().readyState, 3);
  clock.advance(1000);
  assert.equal(sockets.made.length, 2);
});

test('checkpoint_upload_uses_the_ticket_and_base', async () => {
  let answer = new Response(JSON.stringify({ revision: 'r2' }), { status: 200 });
  const { page, fetched, socket } = live({
    routes: { '/api/docs/doc-1/live-checkpoint?': () => answer },
  });
  page.fromEditor({
    type: 'gridcraft:live-checkpoint',
    ticket: TICKET,
    at: 7,
    base: 'r1',
    bytes: new Uint8Array([1, 2]),
    sessionData: new Uint8Array([3]),
    sha256: SHA,
  });
  await settle();
  const call = fetched.calls.at(-1);
  assert.equal(call.url, `/api/docs/doc-1/live-checkpoint?ticket=${TICKET}&at=7&base=r1`);
  assert.equal(call.init.method, 'PUT');
  assert.equal(call.init.headers['x-sharecraft-session-data-length'], '1');
  assert.deepEqual([...new Uint8Array(await call.init.body.arrayBuffer())], [3, 1, 2]);
  assert.equal(socket().sent.length, 0, 'a stored checkpoint needs no frame');

  // No base: none named. A failed upload gives the ticket back, so another writer is asked.
  answer = new Response(null, { status: 503 });
  page.fromEditor({
    type: 'gridcraft:live-checkpoint',
    ticket: TICKET,
    at: 8,
    base: null,
    bytes: new Uint8Array([1]),
    sessionData: new Uint8Array([]),
    sha256: SHA,
  });
  await settle();
  assert.equal(fetched.calls.at(-1).url, `/api/docs/doc-1/live-checkpoint?ticket=${TICKET}&at=8`);
  assert.deepEqual(socket().sent.map((frame) => JSON.parse(frame)), [{ type: 'checkpoint_failed', ticket: TICKET }]);
});

test('hash_is_posted_for_a_verify_ticket', async () => {
  const { page, fetched, socket } = live({
    routes: { '/api/docs/doc-1/live-checkpoint/hash': new Response(null, { status: 202 }) },
  });
  page.fromEditor({ type: 'gridcraft:live-hash', ticket: TICKET, at: 7, sha256: SHA });
  await settle();
  const call = fetched.calls.at(-1);
  assert.equal(call.url, '/api/docs/doc-1/live-checkpoint/hash');
  assert.equal(call.init.method, 'POST');
  assert.equal(call.init.headers['content-type'], 'application/json');
  assert.deepEqual(JSON.parse(call.init.body), { ticket: TICKET, at: 7, sha256: SHA });
  assert.equal(socket().sent.length, 0);
});

test('people_are_passed_to_the_editor_with_their_looks', () => {
  const { page, socket } = live();
  socket().receive(
    JSON.stringify({
      type: 'people',
      people: [{ participant: 2, client: 'c', user: 'u', name: 'Ann Tester', can_edit: true, connected: false }],
    }),
  );
  const people = page.posted.find((message) => message.type === 'gridcraft:live-people');
  assert.deepEqual(people.people, [{ participant: 2, name: 'Ann Tester', initials: 'AT', colour: '#2f5bd3', connected: false }]);
});

test('leave_is_sent_when_the_page_closes', () => {
  const kept = live();
  kept.page.fire('pagehide', { persisted: true });
  assert.deepEqual(kept.socket().sent, [], 'kept in the back/forward cache: nothing is sent');
  assert.equal(kept.page.kinds().includes('gridcraft:live-stop'), false);

  const { page, socket, sockets, clock } = live();
  page.fire('pagehide', { persisted: false });
  assert.deepEqual(socket().sent, ['{"type":"leave"}']);
  assert.equal(page.kinds().at(-1), 'gridcraft:live-stop');
  clock.advance(600_000);
  assert.equal(sockets.made.length, 1, 'no reconnect after leaving');
});

test('watchdog_offers_reload_after_fifteen_silent_seconds', () => {
  const { page, clock, seen } = live();
  const ticks = () => page.kinds().filter((kind) => kind === 'gridcraft:live-tick').length;
  clock.advance(60_000);
  assert.equal(ticks(), 1);
  clock.advance(14_999);
  assert.equal(seen.reloads, 0);
  clock.advance(1);
  assert.equal(seen.reloads, 1, 'reloaded automatically');
  // An answer stops the watchdog for that tick (one short of live keeps the reload count).
  clock.advance(45_000);
  page.fromEditor({ type: 'gridcraft:live-state', phase: 'catching_up', waiting: 0, canEdit: true });
  clock.advance(30_000);
  assert.equal(seen.reloads, 1);
  // Two more silent ticks reload; after three, the page offers "Reload editor" instead.
  for (let n = 0; n < 3; n += 1) clock.advance(60_000);
  assert.equal(seen.reloads, 3);
  assert.equal(seen.offers, 1);
  // A hidden tab is ticked on visibility change but never watched.
  page.ownerDocument.visibilityState = 'hidden';
  page.fireDocument('visibilitychange');
  assert.equal(page.kinds().at(-1), 'gridcraft:live-tick');
  clock.advance(120_000);
  assert.equal(seen.offers, 1);
});

// ---- G5 Task 12 review fixes ----

test('live_retry_keeps_backing_off_while_the_room_is_unavailable', () => {
  let state;
  const waits = [];
  for (let n = 0; n < 12; n += 1) {
    state = liveRetry(state, { type: 'reset', kind: 'unavailable' }, () => 0);
    waits.push(state.wait);
  }
  assert.equal(state.stopped, false, 'an unavailable room is not a client fault');
  assert.equal(waits.at(-1), 60000);
  assert.equal(state.repeats, 0);
  // It does not break a run of identical client errors either.
  let run;
  for (const kind of ['desync', 'desync', 'unavailable', 'desync', 'desync', 'desync']) {
    run = liveRetry(run, { type: 'reset', kind }, () => 0);
  }
  assert.equal(run.stopped, true);
});

/**
 * A page whose editor is reloaded the way file.js does it: the session is closed and a new
 * connection made for the new editor, sharing `memory`.
 */
function reloadingPage() {
  const page = standInPage();
  const clock = standInClock();
  const sockets = standInSockets();
  const memory = {};
  const seen = { reloads: 0, offers: 0, stopped: [], connections: 0 };
  let connection = null;
  const env = {
    WebSocket: sockets.Socket,
    fetch: standInFetch({}).fetch,
    setTimeout: clock.setTimeout,
    clearTimeout: clock.clearTimeout,
    setInterval: clock.setInterval,
    clearInterval: clock.clearInterval,
    random: () => 0,
  };
  const start = () => {
    seen.connections += 1;
    connection = connectLive(page.frame, { prefix: 'gridcraft', live: 'gridcraft/1' }, DOC, {
      canEdit: true,
      memory,
      stopped: (why) => seen.stopped.push(why),
      reloadEditor: () => {
        seen.reloads += 1;
        connection.close();
        start();
      },
      offerReload: () => (seen.offers += 1),
      env,
    });
    sockets.made.at(-1).open();
  };
  start();
  return { page, clock, sockets, seen, socket: () => sockets.made.at(-1) };
}

test('automatic_reloads_are_counted_across_editor_reloads', () => {
  const { page, clock, seen } = reloadingPage();
  // Each new editor is silent: three reloads, then "Reload editor" is offered.
  for (let n = 0; n < 5; n += 1) clock.advance(75_000);
  assert.equal(seen.reloads, 3);
  assert.equal(seen.connections, 4);
  assert.ok(seen.offers >= 1, 'the page offers "Reload editor" after three reloads');
  // Reaching live starts the count again.
  page.fromEditor({ type: 'gridcraft:live-state', phase: 'live', waiting: 0, canEdit: true });
  clock.advance(75_000);
  assert.equal(seen.reloads, 4);
});

test('the_repeated_error_stop_holds_across_editor_reloads', () => {
  const { page, clock, sockets, seen } = reloadingPage();
  for (let n = 0; n < 3; n += 1) {
    page.fromEditor({ type: 'gridcraft:live-reset', kind: 'desync' });
    clock.advance(10_000);
    sockets.made.at(-1).open();
  }
  // The editor stops answering and is reloaded: the run of errors carries on.
  clock.advance(75_000);
  assert.equal(seen.reloads, 1);
  for (let n = 0; n < 2; n += 1) {
    page.fromEditor({ type: 'gridcraft:live-reset', kind: 'desync' });
    clock.advance(10_000);
    sockets.made.at(-1).open();
  }
  assert.deepEqual(seen.stopped, ['repeated']);
});

test('a_close_and_a_reset_together_open_one_socket', () => {
  const { page, socket, sockets, clock } = live();
  // Climb to the one-minute cap.
  for (let n = 0; n < 7; n += 1) {
    page.fromEditor({ type: 'gridcraft:live-reset', kind: `k${n}` });
    clock.advance(60_000);
    socket().open();
  }
  const before = sockets.made.length;
  const closes = page.kinds().filter((kind) => kind === 'gridcraft:live-closed').length;
  // The server closes after its last frame, then the editor asks for a reset.
  socket().close();
  page.fromEditor({ type: 'gridcraft:live-reset', kind: 'expired' });
  clock.advance(120_000);
  assert.equal(sockets.made.length, before + 1, 'one new socket');
  assert.equal(page.kinds().filter((kind) => kind === 'gridcraft:live-closed').length, closes + 1);
});

test('a_late_base_fetch_does_not_reset_a_newer_connection', async () => {
  let fail;
  const pending = new Promise((resolve) => (fail = resolve));
  const { page, socket, sockets, clock } = live({
    routes: { '/api/docs/doc-1/content?revision=r1': () => pending },
  });
  page.fromEditor({ type: 'gridcraft:live-load', revision: 'r1', at: 5 });
  await settle();
  // The connection drops and comes back while the fetch is out.
  socket().close();
  clock.advance(1000);
  assert.equal(sockets.made.length, 2);
  socket().open();
  fail(new Response(null, { status: 503 }));
  await settle();
  assert.equal(socket().readyState, 1, 'the newer connection is kept');
  assert.equal(page.posted.some((message) => message.type === 'gridcraft:live-base'), false);
});

test('leaving_warns_while_live_changes_wait', () => {
  assert.equal(warnOnLeave(undefined), false);
  assert.equal(warnOnLeave({ phase: 'live', waiting: 0, canEdit: true }), false);
  assert.equal(warnOnLeave({ phase: 'offline', waiting: 3, canEdit: true }), true);
  assert.equal(warnOnLeave({ phase: 'live', waiting: 1, canEdit: true }), true);
});

// ---- notices (Task 13) ----

const CTX = { nameOf: (participant) => (participant === 4 ? 'Ann Tester' : null), onlyEditor: false, canManage: false };

test('live_notice_wording_for_each_kind', () => {
  const rows = [
    // The editor's LiveNotice kinds.
    ['editor', { kind: 'not_applied', seq: 3, kept_text: null, reason: 'row_deleted' }, "Your change wasn't applied because someone deleted that row.", 'status'],
    ['editor', { kind: 'not_applied', seq: 3, kept_text: null, reason: 'column_deleted' }, "Your change wasn't applied because someone deleted that column.", 'status'],
    ['editor', { kind: 'not_applied', seq: 3, kept_text: null, reason: 'sheet_deleted' }, "Your change wasn't applied because someone deleted that sheet.", 'status'],
    ['editor', { kind: 'not_applied', seq: 3, kept_text: null, reason: 'target_unavailable' }, "Your change wasn't applied because the cells it was for can't be changed now.", 'status'],
    ['editor', { kind: 'not_applied', seq: 3, kept_text: null, reason: 'changed_since_undo' }, "Undo wasn't applied because someone changed those cells since.", 'status'],
    ['editor', { kind: 'not_applied', seq: 3, kept_text: null, reason: 'invalid' }, "Your change wasn't applied because it isn't valid here.", 'status'],
    ['editor', { kind: 'not_applied', seq: 3, kept_text: null, reason: 'failed' }, "Your change couldn't be applied.", 'status'],
    ['editor', { kind: 'not_applied', seq: 3, kept_text: null, reason: 'something_new' }, "Your change couldn't be applied.", 'status'],
    ['editor', { kind: 'change_refused', reason: 'read_only' }, "You can only view this document, so your change wasn't kept.", 'status'],
    ['editor', { kind: 'change_refused', reason: 'too_large' }, "That change is too large to share, so it wasn't kept. Try a smaller one.", 'status'],
    ['editor', { kind: 'change_refused', reason: 'rate_limited' }, "Changes came too fast, so one wasn't kept. Try it again.", 'status'],
    ['editor', { kind: 'change_refused', reason: 'out_of_order' }, "Your change couldn't be shared, so it wasn't kept.", 'status'],
    ['editor', { kind: 'undo_left', cells: 1 }, 'Undo left 1 cell as it was, because someone changed it since.', 'status'],
    ['editor', { kind: 'undo_left', cells: 3 }, 'Undo left 3 cells as they were, because someone changed them since.', 'status'],
    ['editor', { kind: 'undo_refused', reason: 'anything' }, "Undo isn't possible here, because someone else's changes depend on it.", 'status'],
    ['editor', { kind: 'unshared', command: 'sort' }, 'Not available while editing together.', 'status'],
    ['editor', { kind: 'paused', entries: 10, bytes: 99 }, 'Changes are paused until the latest version is saved. A lot of changes are waiting to go into the stored file.', 'status'],
    ['editor', { kind: 'returned', not_applied: 0 }, 'Welcome back. The changes you made while away have been added.', 'status'],
    ['editor', { kind: 'returned', not_applied: 2 }, "Welcome back. 2 of the changes you made while away couldn't be applied.", 'status'],
    ['editor', { kind: 'returned', not_applied: 1 }, "Welcome back. 1 of the changes you made while away couldn't be applied.", 'status'],
    ['editor', { kind: 'archived_return' }, "You were away too long for your changes to be added. The copy you had is kept in the file's history.", 'status'],
    ['editor', { kind: 'others_returning', participant: 4 }, "Changes Ann Tester made while away are arriving. The version before them is kept.", 'status'],
    ['editor', { kind: 'others_returning', participant: 9 }, "Changes someone made while away are arriving. The version before them is kept.", 'status'],
    ['editor', { kind: 'read_only', read_only: true }, 'You can now only view this document.', 'status'],
    ['editor', { kind: 'read_only', read_only: false }, 'You can edit this document again.', 'status'],
    ['editor', { kind: 'cannot_open' }, "This document can't be opened together, because one of its changes can't be applied. You can download the last stored version.", 'alert'],
    ['editor', { kind: 'incompatible' }, 'This editor is out of date. Reload to keep working together.', 'alert'],
    ['editor', { kind: 'build_refused' }, "Editing together isn't available in the desktop app yet.", 'alert'],
    // The server messages the page peeks at.
    ['server', { type: 'changes_paused', entries: 10, bytes: 99 }, 'Changes are paused until the latest version is saved. A lot of changes are waiting to go into the stored file.', 'status'],
    ['server', { type: 'returning', participant: 4 }, "Changes Ann Tester made while away are arriving. The version before them is kept.", 'status'],
    ['server', { type: 'cannot_open', seq: 5 }, "This document can't be opened together, because one of its changes can't be applied. You can download the last stored version.", 'alert'],
    ['server', { type: 'incompatible', protocol: 1, format: 'gridcraft/1' }, 'This editor is out of date. Reload to keep working together.', 'alert'],
    ['server', { type: 'build_refused', build: {} }, "Editing together isn't available in the desktop app yet.", 'alert'],
    // Resets: joining again, never an error.
    ['reset', 'unavailable', 'This document is busy right now. Joining again shortly; your changes are kept.', 'status'],
    ['reset', 'expired', 'You were away for a while. Joining again; your changes are kept.', 'status'],
    ['reset', 'base', "The latest version couldn't be loaded. Trying again shortly.", 'status'],
    ['reset', 'base_fetch', "The latest version couldn't be loaded. Trying again shortly.", 'status'],
    ['reset', 'overloaded', 'Too many changes arrived at once. Reconnecting; your changes are kept.', 'status'],
    ['reset', 'editor', 'Reconnecting; your changes are kept.', 'status'],
    ['reset', 'malformed', 'Reconnecting; your changes are kept.', 'status'],
    ['reset', 'out_of_order', 'Reconnecting; your changes are kept.', 'status'],
    ['reset', 'desync', 'Reconnecting; your changes are kept.', 'status'],
    ['reset', 'agreed_unavailable', 'Reconnecting; your changes are kept.', 'status'],
    // Stops.
    ['stop', 'repeated', "This document can't be opened together right now.", 'alert'],
    ['stop', 'incompatible', 'This editor is out of date. Reload to keep working together.', 'alert'],
    ['stop', 'build_refused', "Editing together isn't available in the desktop app yet.", 'alert'],
    ['stop', 'cannot_open', "This document can't be opened together, because one of its changes can't be applied. You can download the last stored version.", 'alert'],
    ['stop', 'not_answering', "The editor isn't answering.", 'alert'],
  ];
  for (const [source, value, text, role] of rows) {
    const notice = liveNotice(source, value, CTX);
    assert.equal(notice?.text, text, `${source} ${JSON.stringify(value)}`);
    assert.equal(notice.role, role, `${source} ${JSON.stringify(value)}`);
    assert.ok(notice.actions.length <= 2);
  }
  // The stops offer their way on; "Reload editor" is its own button.
  assert.deepEqual(liveNotice('stop', 'not_answering', CTX).actions, ['reloadEditor']);
  assert.deepEqual(liveNotice('stop', 'repeated', CTX).actions, ['reload']);
  assert.deepEqual(liveNotice('stop', 'incompatible', CTX).actions, ['reload']);
  assert.deepEqual(liveNotice('stop', 'cannot_open', CTX).actions, ['download']);
  // Resumed clears the pause; people, access, removed, expired and saved say nothing here.
  assert.deepEqual(liveNotice('editor', { kind: 'resumed' }, CTX), { topic: 'paused', clear: true });
  assert.deepEqual(liveNotice('server', { type: 'changes_resumed' }, CTX), { topic: 'paused', clear: true });
  for (const type of ['people', 'access', 'removed', 'expired']) assert.equal(liveNotice('server', { type }, CTX), null);
  assert.equal(liveNotice('server', { type: 'saved', revision: 'r', through: 3, verified: true, major: false }, CTX), null);
  assert.equal(liveNotice('editor', { kind: 'removed' }, CTX), null);
  assert.equal(liveNotice('editor', { kind: 'nonsense' }, CTX), null);
  assert.equal(liveNotice('editor', null, CTX), null);
});

test('not_applied_notice_offers_to_copy_the_kept_text', () => {
  const notice = liveNotice('editor', { kind: 'not_applied', seq: 3, kept_text: '<b>=SUM(A1)</b>', reason: 'row_deleted' }, CTX);
  assert.equal(notice.text, "Your change wasn't applied because someone deleted that row. Your text is kept.");
  assert.deepEqual(notice.actions, ['copy']);
  assert.equal(notice.copy, '<b>=SUM(A1)</b>', 'kept as text, to copy; never shown as markup');
  assert.ok(!notice.text.includes('SUM'), 'the typed text is not in the sentence');
  const none = liveNotice('editor', { kind: 'not_applied', seq: 3, kept_text: '', reason: 'row_deleted' }, CTX);
  assert.deepEqual(none.actions, []);
});

test('paused_notice_asks_to_keep_the_tab_open_only_when_needed', () => {
  const paused = { kind: 'paused', entries: 10, bytes: 99 };
  assert.doesNotMatch(liveNotice('editor', paused, CTX).text, /Keep this tab open/);
  const alone = liveNotice('editor', paused, { ...CTX, onlyEditor: true });
  assert.match(alone.text, /Keep this tab open so they can be saved\.$/);
  assert.match(liveNotice('server', { type: 'changes_paused' }, { ...CTX, onlyEditor: true }).text, /Keep this tab open/);
  // Only an editor that may save counts; viewers and dropped places do not.
  const people = [
    { participant: 1, can_edit: true, connected: true },
    { participant: 2, can_edit: false, connected: true },
    { participant: 3, can_edit: true, connected: false },
  ];
  assert.equal(onlyEditor(people, true), true);
  assert.equal(onlyEditor(people, false), false);
  assert.equal(onlyEditor([...people, { participant: 4, can_edit: true, connected: true }], true), false);
  assert.equal(onlyEditor(null, true), true);
});

test('returning_notice_says_the_earlier_version_is_kept', () => {
  const notice = liveNotice('server', { type: 'returning', participant: 4 }, CTX);
  assert.match(notice.text, /^Changes Ann Tester made while away are arriving\./);
  assert.match(notice.text, /The version before them is kept\.$/);
  assert.equal(notice.topic, 'return');
});

test('owner_sees_the_checkpoint_mismatch_notice', () => {
  const saved = { type: 'saved', revision: 'r', through: 3, verified: false, major: false, mismatch: true };
  assert.equal(liveNotice('server', saved, CTX), null, 'editors and viewers see nothing');
  const owner = liveNotice('server', saved, { ...CTX, canManage: true });
  assert.equal(owner.text, 'Two editors saved different files for the same changes. Both are in the history.');
  assert.deepEqual(owner.actions, ['activity']);
  assert.equal(liveNotice('server', { ...saved, mismatch: false }, { ...CTX, canManage: true }), null);
});

test('one_banner_at_a_time_with_at_most_two_actions', () => {
  const change = liveNotice('editor', { kind: 'not_applied', seq: 1, kept_text: 'x', reason: 'row_deleted' }, CTX);
  const paused = liveNotice('editor', { kind: 'paused', entries: 1, bytes: 1 }, CTX);
  const stop = liveNotice('stop', 'repeated', CTX);
  // The newest wins.
  assert.equal(pickBanner(change, paused), paused);
  assert.equal(pickBanner(null, change), change);
  // A stop is not replaced by information, only by another stop.
  assert.equal(pickBanner(stop, change), stop);
  const other = liveNotice('stop', 'incompatible', CTX);
  assert.equal(pickBanner(stop, other), other);
  // A clear takes away only its own topic.
  const resumed = liveNotice('editor', { kind: 'resumed' }, CTX);
  assert.equal(pickBanner(paused, resumed), null);
  assert.equal(pickBanner(change, resumed), change);
  assert.equal(pickBanner(paused, null), paused);
  // At most two actions, whatever is asked.
  assert.equal(pickBanner(null, { topic: 'x', text: 't', role: 'status', actions: ['a', 'b', 'c'] }).actions.length, 2);
});

test('notices_from_the_editor_are_bounded', () => {
  const parse = (notice) => parseLiveEditorMessage('gridcraft', { type: 'gridcraft:live-notice', notice });
  // Only the fields of the kind are kept, each checked.
  const kept = parse({ kind: 'not_applied', seq: 3, kept_text: 'hi', reason: 'row_deleted', extra: 'x'.repeat(10) });
  assert.deepEqual(kept.notice, { kind: 'not_applied', seq: 3, kept_text: 'hi', reason: 'row_deleted' });
  // Text over the limit is dropped, not shown cut.
  const long = parse({ kind: 'not_applied', seq: 3, kept_text: 'x'.repeat(40_000), reason: 'row_deleted' });
  assert.equal(long.notice.kept_text, null);
  assert.equal(parse({ kind: 'undo_left', cells: -1 }), null);
  assert.equal(parse({ kind: 'others_returning', participant: 'x' }), null);
  assert.deepEqual(parse({ kind: 'read_only', read_only: true }).notice, { kind: 'read_only', read_only: true });
  assert.equal(parse({ kind: 'read_only' }), null);
  assert.equal(parse({ kind: 'unknown_kind' }), null);
});

test('the_send_limit_counts_bytes', () => {
  const parse = (frame) => parseLiveEditorMessage('gridcraft', { type: 'gridcraft:live-send', frame });
  assert.ok(parse('a'.repeat(MAX_SEND_BYTES)));
  assert.equal(parse('a'.repeat(MAX_SEND_BYTES + 1)), null);
  // Three bytes each in UTF-8: fewer than the limit in characters, over it in bytes.
  assert.equal(parse('€'.repeat(Math.floor(MAX_SEND_BYTES / 3) + 1)), null);
  assert.ok(parse('€'.repeat(Math.floor(MAX_SEND_BYTES / 3))));
  // A pair of surrogates is four bytes; a lone one is three, as TextEncoder writes it.
  assert.ok(parse('😀'.repeat(MAX_SEND_BYTES / 4)));
  assert.equal(parse('😀'.repeat(MAX_SEND_BYTES / 4) + 'a'), null);
});

test('checkpoint_sizes_are_bounded_before_copying', () => {
  const checkpoint = (bytes, sessionData) =>
    parseLiveEditorMessage('gridcraft', { type: 'gridcraft:live-checkpoint', ticket: TICKET, at: 1, base: null, bytes, sessionData, sha256: SHA });
  // The size is read from what the editor posted, before anything is copied (a stand-in view
  // claims a size over the limit; a copy of it would be one byte and pass).
  const claiming = (size) => {
    const view = new Uint8Array(1);
    Object.defineProperty(view, 'byteLength', { get: () => size });
    return view;
  };
  assert.equal(checkpoint(claiming(512 * 1024 * 1024 + 1), new Uint8Array(0)), null);
  assert.equal(checkpoint(new Uint8Array(1), claiming(8 * 1024 * 1024 + 1)), null);
  assert.ok(checkpoint(claiming(512 * 1024 * 1024), claiming(8 * 1024 * 1024)));
  assert.ok(checkpoint(new Uint8Array(2), new Uint8Array(1)));
});

test('the_page_hears_resets_saves_and_its_own_changes', async () => {
  const resets = [];
  const saving = [];
  let changed = 0;
  const { page, socket } = live({
    routes: { '/api/docs/doc-1/live-checkpoint?': new Response(null, { status: 200 }) },
    handlers: { reset: (kind) => resets.push(kind), saving: (on) => saving.push(on), changed: () => (changed += 1) },
  });
  page.fromEditor({ type: 'gridcraft:live-send', frame: '{"type":"entry","local":1,"payload":"x"}' });
  page.fromEditor({ type: 'gridcraft:live-send', frame: '{"type":"presence","payload":"{\\"type\\":\\"entry\\"}"}' });
  page.fromEditor({ type: 'gridcraft:live-send', frame: '{"type":"heartbeat","seen":1}' });
  assert.equal(changed, 1, 'only an entry counts as a change; presence and heartbeats do not');
  assert.equal(socket().sent.length, 3);
  page.fromEditor({ type: 'gridcraft:live-checkpoint', ticket: TICKET, at: 1, base: null, bytes: new Uint8Array(1), sessionData: new Uint8Array(0), sha256: SHA });
  assert.deepEqual(saving, [true]);
  await settle();
  assert.deepEqual(saving, [true, false]);
  page.fromEditor({ type: 'gridcraft:live-reset', kind: 'unavailable' });
  assert.deepEqual(resets, ['unavailable']);
});
