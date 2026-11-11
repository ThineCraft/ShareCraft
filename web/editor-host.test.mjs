import test from 'node:test';
import assert from 'node:assert/strict';
import {
  BRIDGES,
  SaveQueue,
  connect,
  classifySave,
  editorUrl,
  nextStatus,
  parseHostMessage,
  saveFailure,
  withExtension,
} from './editor-host.js';

test('exports_are_not_saves', () => {
  assert.equal(classifySave('.xlsx', 'Book.xlsx'), 'save');
  assert.equal(classifySave('.xlsx', 'BOOK.XLSX'), 'save');
  assert.equal(classifySave('.solidcraft.json', 'Part.SolidCraft.JSON'), 'save');
  for (const name of ['Book.pdf', 'Book.csv', 'Book.xlsx.html', '', undefined, null, 42]) {
    assert.equal(classifySave('.xlsx', name), 'export', JSON.stringify(name));
  }
  // Only ASCII letters are folded: the Kelvin sign is not a "k".
  assert.equal(classifySave('.kml', 'Map.Kml'), 'export');
});

test('messages_with_another_prefix_are_ignored', () => {
  const bytes = new ArrayBuffer(3);
  assert.equal(parseHostMessage('gridcraft', { type: 'wordcraft:save', name: 'a.xlsx', bytes }), null);
  assert.equal(parseHostMessage('gridcraft', { type: 'gridcraftx:ready', version: '1' }), null);
  assert.equal(parseHostMessage('gridcraft', { type: 'grid:ready', version: '1' }), null);
  assert.equal(parseHostMessage('gridcraft', { type: 'ready', version: '1' }), null);
  // Kinds the page sends, or does not know, are not the editor's.
  assert.equal(parseHostMessage('gridcraft', { type: 'gridcraft:open', name: 'a', bytes }), null);
  assert.equal(parseHostMessage('gridcraft', { type: 'gridcraft:handoff', name: 'a', bytes }), null);
  assert.deepEqual(parseHostMessage('gridcraft', { type: 'gridcraft:ready', version: '1' }), {
    kind: 'ready',
    version: '1',
  });
  assert.deepEqual(parseHostMessage('gridcraft', { type: 'gridcraft:dirty', dirty: true }), {
    kind: 'dirty',
    dirty: true,
  });
  assert.deepEqual(parseHostMessage('gridcraft', { type: 'gridcraft:failed', error: 'boom' }), {
    kind: 'failed',
    error: 'boom',
  });
});

test('malformed_messages_are_ignored', () => {
  for (const data of [null, undefined, 'gridcraft:ready', 7, ['gridcraft:ready'], {}, { type: 3 }]) {
    assert.equal(parseHostMessage('gridcraft', data), null, JSON.stringify(data));
  }
  for (const bytes of [undefined, null, 'abc', [1, 2, 3], { length: 2 }, new Uint16Array(2), new DataView(new ArrayBuffer(2))]) {
    assert.equal(
      parseHostMessage('gridcraft', { type: 'gridcraft:save', name: 'a.xlsx', bytes }),
      null,
      String(bytes),
    );
  }
  assert.equal(parseHostMessage('gridcraft', { type: 'gridcraft:dirty', dirty: 'yes' }), null);
  assert.equal(parseHostMessage('gridcraft', { type: 'gridcraft:dirty' }), null);
});

test('saves_carry_a_copy_of_the_bytes', () => {
  const buffer = new Uint8Array([1, 2, 3]).buffer;
  const fromBuffer = parseHostMessage('gridcraft', { type: 'gridcraft:save', name: 'a.xlsx', bytes: buffer });
  assert.equal(fromBuffer.kind, 'save');
  assert.equal(fromBuffer.name, 'a.xlsx');
  assert.deepEqual([...fromBuffer.bytes], [1, 2, 3]);
  new Uint8Array(buffer)[0] = 9;
  assert.equal(fromBuffer.bytes[0], 1, 'the page keeps its own copy');

  const view = new Uint8Array([5, 6, 7, 8]).subarray(1, 3);
  const fromView = parseHostMessage('gridcraft', { type: 'gridcraft:save', bytes: view, saveAs: true });
  assert.deepEqual([...fromView.bytes], [6, 7]);
  assert.equal(fromView.name, '', 'a missing name is empty');
  assert.equal(fromView.saveAs, true);
});

test('editor_url_encodes_the_author', () => {
  assert.equal(
    editorUrl('gridcraft', 'Ann & Bob #1 Jürgen'),
    '/editors/gridcraft/index.html?host=parent&author=Ann%20%26%20Bob%20%231%20J%C3%BCrgen',
  );
  assert.equal(editorUrl('pdfcraft', 'Zoë'), '/editors/pdfcraft/index.html?host=parent&control&author=Zo%C3%AB');
  // SolidCraft takes no author: the name stays out of an address where it does nothing.
  assert.equal(editorUrl('solidcraft', 'Ann & Bob'), '/editors/solidcraft/index.html?embed=editor');
  assert.equal(editorUrl('text', 'A'), null);
  assert.equal(editorUrl('__proto__', 'A'), null);
  assert.equal(BRIDGES.solidcraft.kind, 'solidcraft-request');
});

test('status_follows_events', () => {
  const start = nextStatus(undefined, { type: 'opening' });
  // [events from the start, expected words]
  const table = [
    [[], 'Opening…'],
    [[{ type: 'opened', canSave: true }], 'Saved'],
    [[{ type: 'opened', canSave: false }], 'View only'],
    [[{ type: 'opened', canSave: false }, { type: 'dirty', dirty: true }], 'View only'],
    [[{ type: 'opened', canSave: true }, { type: 'dirty', dirty: true }], 'Unsaved changes'],
    [[{ type: 'opened', canSave: true }, { type: 'dirty', dirty: true }, { type: 'saving' }], 'Saving…'],
    [[{ type: 'opened', canSave: true }, { type: 'saving' }, { type: 'stored' }], 'Saved'],
    [[{ type: 'opened', canSave: true }, { type: 'saving' }, { type: 'stored' }, { type: 'dirty', dirty: true }], 'Unsaved changes'],
    [[{ type: 'opened', canSave: true }, { type: 'saving' }, { type: 'not_stored' }], 'Not saved'],
    [[{ type: 'opened', canSave: true }, { type: 'saving' }, { type: 'not_stored' }, { type: 'dirty', dirty: true }], 'Not saved'],
    [[{ type: 'opened', canSave: true }, { type: 'saving' }, { type: 'not_stored' }, { type: 'saving' }, { type: 'stored' }], 'Saved'],
    [[{ type: 'opened', canSave: true }, { type: 'offline' }], 'Connection lost. Saving is paused.'],
    [[{ type: 'opening' }, { type: 'offline' }], 'Connection lost. Saving is paused.'],
    [[{ type: 'opened', canSave: true }, { type: 'dirty', dirty: true }, { type: 'offline' }, { type: 'online' }], 'Unsaved changes'],
    [[{ type: 'opened', canSave: true }, { type: 'seat', canSave: false }], 'View only'],
    [[{ type: 'opened', canSave: true }, { type: 'dirty', dirty: true }, { type: 'dirty', dirty: false }], 'Saved'],
  ];
  for (const [events, words] of table) {
    let state = start;
    for (const event of events) state = nextStatus(state, event);
    assert.equal(state.text, words, JSON.stringify(events));
    assert.equal(typeof state.tone, 'string');
  }
  // nextStatus never changes the state it is given.
  const opened = nextStatus(start, { type: 'opened', canSave: true });
  nextStatus(opened, { type: 'dirty', dirty: true });
  assert.equal(opened.text, 'Saved');
});

test('new_files_get_their_extension', () => {
  assert.equal(withExtension('Budget', '.xlsx'), 'Budget.xlsx');
  assert.equal(withExtension('  Budget  ', '.xlsx'), 'Budget.xlsx');
  assert.equal(withExtension('Budget.XLSX', '.xlsx'), 'Budget.XLSX');
  assert.equal(withExtension('', '.docx'), 'Untitled.docx');
  assert.equal(withExtension('Part', '.solidcraft.json'), 'Part.solidcraft.json');
});

// A stand-in upload: every call waits until the test answers it.
function standIn() {
  const calls = [];
  const upload = (bytes) =>
    new Promise((resolve) => {
      calls.push({ bytes, answer: resolve });
    });
  return { calls, upload };
}

const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

test('refused_save_keeps_the_bytes', async () => {
  const { calls, upload } = standIn();
  const events = [];
  const queue = new SaveQueue(upload, (event) => events.push(event.type));
  const first = new Uint8Array([1]);
  queue.offer(first);
  assert.equal(calls.length, 1);
  assert.equal(calls[0].bytes, first);
  assert.equal(queue.kept, first);

  // (A 409 stops saving instead: see a_conflict_stops_saving.)
  calls[0].answer({ ok: false, status: 503 });
  await settle();
  assert.equal(queue.kept, first, 'a refused save keeps its bytes');
  assert.equal(queue.failure.status, 503);
  assert.equal(calls.length, 1, 'nothing is uploaded again by itself');

  queue.retry();
  assert.equal(calls.length, 2);
  assert.equal(calls[1].bytes, first, 'the same bytes are offered again');
  assert.equal(queue.kept, first);
  calls[1].answer({ ok: true });
  await settle();
  assert.equal(queue.kept, null, 'released only after the server stored them');
  assert.equal(queue.failure, null);
  assert.deepEqual(events, ['saving', 'not_stored', 'saving', 'stored']);

  // A newer save replaces a waiting one: at most one waits.
  const [a, b, c] = [new Uint8Array([2]), new Uint8Array([3]), new Uint8Array([4])];
  queue.offer(a);
  queue.offer(b);
  queue.offer(c);
  assert.equal(calls.length, 3);
  assert.equal(calls[2].bytes, a);
  assert.equal(queue.kept, c, 'the newest bytes are the ones kept');
  calls[2].answer({ ok: true });
  await settle();
  assert.equal(calls.length, 4);
  assert.equal(calls[3].bytes, c, 'b was replaced by c and never sent');
  assert.equal(queue.kept, c);
  calls[3].answer({ ok: true });
  await settle();
  assert.equal(queue.kept, null);
  assert.equal(calls.length, 4);
});

test('a_paused_queue_waits_and_a_dropped_one_forgets', async () => {
  const { calls, upload } = standIn();
  const queue = new SaveQueue(upload);
  queue.pause();
  const a = new Uint8Array([1]);
  queue.offer(a);
  assert.equal(calls.length, 0, 'nothing is uploaded while paused');
  assert.equal(queue.kept, a);
  queue.resume();
  assert.equal(calls.length, 1);
  queue.drop();
  assert.equal(queue.kept, null);
  calls[0].answer({ ok: false, status: 503 });
  await settle();
  assert.equal(queue.kept, null, 'an answer after drop keeps nothing');
  queue.offer(new Uint8Array([2]));
  assert.equal(calls.length, 1, 'a dropped queue takes nothing more');

  // A failed upload that rejects is a failure, not an unhandled rejection.
  const failing = new SaveQueue(() => Promise.reject(new Error('network')));
  const b = new Uint8Array([3]);
  failing.offer(b);
  await settle();
  assert.equal(failing.kept, b);
  assert.equal(failing.failure.status, 0);
});

test('the_in_flight_save_fails_while_a_newer_one_waits', async () => {
  const { calls, upload } = standIn();
  const events = [];
  const queue = new SaveQueue(upload, (event) => events.push(event.type));
  const [a, b] = [new Uint8Array([1]), new Uint8Array([2])];
  queue.offer(a);
  queue.offer(b);
  calls[0].answer({ ok: false, status: 503 });
  await settle();
  assert.equal(calls.length, 2, 'the newer save is sent');
  assert.equal(calls[1].bytes, b);
  assert.equal(queue.kept, b, 'the newer bytes are the ones kept');
  assert.equal(queue.failure, null, 'nothing is left to retry: the newer save carries it all');
  calls[1].answer({ ok: true });
  await settle();
  assert.equal(queue.kept, null);
  assert.deepEqual(events, ['saving', 'not_stored', 'saving', 'stored']);
});

test('a_conflict_stops_saving', async () => {
  const { calls, upload } = standIn();
  const queue = new SaveQueue(upload);
  const [a, b] = [new Uint8Array([1]), new Uint8Array([2])];
  queue.offer(a);
  calls[0].answer({ ok: false, status: 409, current: 'elsewhere' });
  await settle();
  assert.equal(queue.stopped, true);
  assert.equal(queue.kept, a);
  queue.retry();
  queue.resume();
  assert.equal(calls.length, 1, 'nothing is uploaded after a conflict');
  queue.offer(b);
  assert.equal(calls.length, 1, 'a later save is kept, not uploaded');
  assert.equal(queue.kept, b, 'the newest copy is the one kept');
});

test('failures_say_what_happened_and_what_can_be_done', () => {
  const conflict = saveFailure({ status: 409 }, true);
  assert.equal(conflict.stop, true);
  assert.deepEqual(conflict.actions, ['mycopy', 'reload']);
  assert.match(conflict.text, /changed somewhere else/);
  for (const status of [403, 413]) {
    const failure = saveFailure({ status }, true);
    assert.equal(failure.stop, false);
    assert.deepEqual(failure.actions, ['mycopy'], String(status));
  }
  for (const result of [{ status: 503 }, { status: 503, error: 'busy' }, { status: 0 }, { status: 500 }]) {
    const failure = saveFailure(result, true);
    assert.equal(failure.stop, false);
    assert.deepEqual(failure.actions, ['retry', 'mycopy'], JSON.stringify(result));
    // Without anything the queue can retry, Try again is not offered.
    assert.deepEqual(saveFailure(result, false).actions, ['mycopy'], JSON.stringify(result));
  }
  assert.notEqual(saveFailure({ status: 503, error: 'busy' }, true).text, saveFailure({ status: 503 }, true).text);
  for (const failure of [409, 403, 413, 503, 0].map((status) => saveFailure({ status }, true))) {
    assert.match(failure.text, /not stored/);
  }
});

// ---- the SolidCraft bridge, with a stand-in editor and frame ----

const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const FAST = { ...BRIDGES.solidcraft, readyEvery: 5, readyWithin: 2000, pollEvery: 10, answerWithin: 200 };

// A stand-in `window.solidcraft`: `answers[method]` decides each request's answer (a value, a
// function of the params that may throw, or "hang" for a request that never settles).
function standInEditor(answers = {}) {
  const calls = [];
  const solidcraft = {
    request(method, params) {
      calls.push({ method, params });
      const answer = answers[method];
      if (answer === 'hang') return new Promise(() => {});
      return new Promise((resolve, reject) => {
        setTimeout(() => {
          try {
            resolve(typeof answer === 'function' ? answer(params) : answer);
          } catch (err) {
            reject(err);
          }
        }, 1);
      });
    },
  };
  const count = (method) => calls.filter((call) => call.method === method).length;
  return { solidcraft, calls, count };
}

function standInFrame() {
  const contentWindow = {};
  const frame = { contentWindow, ownerDocument: { visibilityState: 'visible', defaultView: globalThis } };
  return { frame, appear: (solidcraft) => (contentWindow.solidcraft = solidcraft) };
}

function recorder() {
  const seen = { ready: 0, save: [], dirty: [], failed: [] };
  const handlers = {
    ready: () => seen.ready++,
    save: (message) => seen.save.push(message),
    dirty: (message) => seen.dirty.push(message.dirty),
    failed: (message) => seen.failed.push(message.error),
  };
  return { seen, handlers };
}

const DESIGN = '{"design":"é"}';
const exported = (revision, dirty = true) => ({ name: 'Bracket', revision, dirty, format: 'json', text: DESIGN });

test('import_is_sent_after_the_interface_appears', async () => {
  const { frame, appear } = standInFrame();
  const { seen, handlers } = recorder();
  const editor = standInEditor({ 'document.import': { revision: 1 }, 'document.export': exported(1, false) });
  const host = connect(frame, FAST, handlers);
  await wait(30);
  assert.equal(seen.ready, 0, 'not ready before the interface exists');
  appear(editor.solidcraft);
  await wait(30);
  assert.equal(seen.ready, 1);
  host.open('Bracket.solidcraft.json', new TextEncoder().encode(DESIGN));
  await wait(20);
  const imported = editor.calls.find((call) => call.method === 'document.import');
  assert.deepEqual(imported.params, { text: DESIGN, name: 'Bracket' });
  assert.deepEqual(seen.failed, []);
  host.close();
});

test('save_confirms_only_after_the_upload', async () => {
  const { frame, appear } = standInFrame();
  const { seen, handlers } = recorder();
  const editor = standInEditor({
    'document.export': exported(7),
    'document.saved': (p) => ({ revision: p.revision, dirty: false }),
  });
  appear(editor.solidcraft);
  const host = connect(frame, { ...FAST, pollEvery: 100000 }, handlers);
  await wait(20);
  assert.equal(host.requestSave(), true);
  await wait(20);
  assert.equal(seen.save.length, 1);
  const save = seen.save[0];
  assert.equal(classifySave('.solidcraft.json', save.name), 'save');
  assert.equal(new TextDecoder().decode(save.bytes), DESIGN);
  // The upload fails (the page never says it was stored): the editor is not told it is saved.
  await wait(20);
  assert.equal(editor.count('document.saved'), 0);
  // Stored: now it is told, with the revision the export carried.
  host.stored(save.bytes);
  await wait(20);
  assert.deepEqual(
    editor.calls.filter((c) => c.method === 'document.saved').map((c) => c.params),
    [{ revision: 7 }],
  );
  // Bytes it did not hand over confirm nothing.
  host.stored(new Uint8Array([1]));
  await wait(20);
  assert.equal(editor.count('document.saved'), 1);
  host.close();
});

test('a_refused_confirmation_leaves_it_unsaved', async () => {
  const { frame, appear } = standInFrame();
  const { seen, handlers } = recorder();
  const editor = standInEditor({
    'document.export': exported(3),
    'document.saved': () => {
      throw new Error('the design changed since revision 3 (now 4)');
    },
  });
  appear(editor.solidcraft);
  const host = connect(frame, { ...FAST, pollEvery: 100000 }, handlers);
  await wait(20);
  host.requestSave();
  await wait(20);
  host.stored(seen.save[0].bytes);
  await wait(20);
  assert.equal(editor.count('document.saved'), 1);
  assert.deepEqual(seen.dirty, [true], 'the page hears it still has unsaved changes');
  assert.deepEqual(seen.failed, [], 'a design changed meanwhile is not an editor failure');
  host.close();
});

test('non_utf8_content_is_an_error', async () => {
  const { frame, appear } = standInFrame();
  const { seen, handlers } = recorder();
  const editor = standInEditor({ 'document.import': { revision: 1 }, 'document.export': exported(1, false) });
  appear(editor.solidcraft);
  const host = connect(frame, FAST, handlers);
  await wait(20);
  host.open('Bracket.solidcraft.json', new Uint8Array([0x7b, 0xff, 0xfe, 0x7d]));
  await wait(20);
  assert.equal(seen.failed.length, 1);
  assert.equal(editor.count('document.import'), 0, 'not sent');
  host.close();
});

test('polling_stops_when_disconnected', async () => {
  const { frame, appear } = standInFrame();
  const { seen, handlers } = recorder();
  const editor = standInEditor({ 'document.export': exported(2, true) });
  appear(editor.solidcraft);
  const host = connect(frame, FAST, handlers);
  await wait(80);
  const polls = editor.count('document.export');
  assert.ok(polls >= 2, `polled ${polls} times`);
  assert.ok(seen.dirty.includes(true), 'dirty is reported');
  host.close();
  const before = editor.calls.length;
  await wait(60);
  assert.equal(editor.calls.length, before, 'no request after close');

  // A hidden tab is not polled.
  const hidden = standInFrame();
  const quiet = standInEditor({ 'document.export': exported(2, true) });
  hidden.appear(quiet.solidcraft);
  hidden.frame.ownerDocument.visibilityState = 'hidden';
  const other = connect(hidden.frame, FAST, recorder().handlers);
  await wait(60);
  assert.equal(quiet.calls.length, 0);
  other.close();
});

test('polls_never_overlap', async () => {
  const { frame, appear } = standInFrame();
  const { handlers } = recorder();
  // Each export takes longer than the polling interval.
  const slow = { calls: 0, running: 0, most: 0 };
  appear({
    request(method) {
      if (method !== 'document.export') return Promise.resolve({});
      slow.calls++;
      slow.running++;
      slow.most = Math.max(slow.most, slow.running);
      return new Promise((resolve) =>
        setTimeout(() => {
          slow.running--;
          resolve(exported(1, false));
        }, 30),
      );
    },
  });
  const host = connect(frame, { ...FAST, pollEvery: 5 }, handlers);
  await wait(150);
  host.close();
  assert.ok(slow.calls >= 2, `polled ${slow.calls} times`);
  assert.equal(slow.most, 1);
});

test('a_request_timeout_is_reported_and_marks_nothing_stored', async () => {
  const { frame, appear } = standInFrame();
  const { seen, handlers } = recorder();
  const editor = standInEditor({ 'document.export': 'hang' });
  appear(editor.solidcraft);
  const host = connect(frame, { ...FAST, pollEvery: 100000, answerWithin: 30 }, handlers);
  await wait(20);
  host.requestSave();
  await wait(80);
  assert.equal(seen.save.length, 0, 'nothing reaches the save path');
  assert.equal(seen.failed.length, 1);
  assert.match(seen.failed[0], /did not answer/);
  assert.equal(editor.count('document.saved'), 0);
  host.close();
});

test('ready_survives_a_window_that_throws_or_is_not_there_yet', async () => {
  const { seen, handlers } = recorder();
  const editor = standInEditor({ 'document.import': { revision: 1 }, 'document.export': exported(1, false) });
  let reads = 0;
  const frame = {
    ownerDocument: { visibilityState: 'visible', defaultView: globalThis },
    get contentWindow() {
      reads++;
      if (reads === 1) throw new Error('SecurityError: a cross-origin window');
      if (reads === 2) return null;
      return { solidcraft: editor.solidcraft };
    },
  };
  const host = connect(frame, FAST, handlers);
  await wait(60);
  assert.equal(seen.ready, 1, 'found once the window lets it be read');
  host.open('Bracket.solidcraft.json', new TextEncoder().encode(DESIGN));
  await wait(20);
  assert.equal(editor.count('document.import'), 1, 'the import is still sent');
  host.close();
});

test('ready_detection_gives_up_after_its_time', async () => {
  const { seen, handlers } = recorder();
  let reads = 0;
  const frame = {
    ownerDocument: { visibilityState: 'visible', defaultView: globalThis },
    get contentWindow() {
      reads++;
      throw new Error('SecurityError');
    },
  };
  const host = connect(frame, { ...FAST, readyWithin: 40 }, handlers);
  await wait(80);
  const after = reads;
  assert.ok(after >= 2, `looked ${after} times`);
  await wait(40);
  assert.equal(reads, after, 'it stopped looking');
  assert.equal(seen.ready, 0);
  assert.deepEqual(seen.failed, [], "the page's own ready timeout reports it");
  host.close();
});

test('the_not_answering_failure_is_marked', async () => {
  const { frame, appear } = standInFrame();
  const failures = [];
  const editor = standInEditor({ 'document.export': 'hang' });
  appear(editor.solidcraft);
  const host = connect(frame, { ...FAST, pollEvery: 100000, answerWithin: 20 }, {
    failed: (message) => failures.push(message),
  });
  await wait(20);
  host.requestSave();
  await wait(60);
  assert.equal(failures.length, 1);
  assert.equal(failures[0].notAnswering, true);
  assert.equal(failures[0].error, 'The editor did not answer. Show this tab and try again.');
  host.close();
});

test('overlapping_saves_confirm_their_own_revisions', async () => {
  const { frame, appear } = standInFrame();
  const { seen, handlers } = recorder();
  let revision = 4;
  const editor = standInEditor({
    'document.export': () => exported(++revision),
    'document.saved': (p) => {
      if (p.revision === 5) throw new Error('the design changed since revision 5 (now 6)');
      return { revision: p.revision, dirty: false };
    },
  });
  appear(editor.solidcraft);
  const host = connect(frame, { ...FAST, pollEvery: 100000 }, handlers);
  await wait(20);
  host.requestSave();
  host.requestSave();
  await wait(20);
  assert.equal(seen.save.length, 2);
  host.stored(seen.save[0].bytes);
  await wait(20);
  host.stored(seen.save[1].bytes);
  await wait(20);
  assert.deepEqual(
    editor.calls.filter((c) => c.method === 'document.saved').map((c) => c.params.revision),
    [5, 6],
  );
  assert.deepEqual(seen.dirty, [true], 'the refused first confirmation reports unsaved');
  host.close();
});

test('a_late_export_after_a_timeout_is_not_saved', async () => {
  const { frame, appear } = standInFrame();
  const { seen, handlers } = recorder();
  appear({
    request: (method) =>
      method === 'document.export'
        ? new Promise((resolve) => setTimeout(() => resolve(exported(9)), 60))
        : Promise.resolve({}),
  });
  const host = connect(frame, { ...FAST, pollEvery: 100000, answerWithin: 20 }, handlers);
  await wait(20);
  host.requestSave();
  await wait(120);
  assert.equal(seen.failed.length, 1);
  assert.equal(seen.save.length, 0, 'the late answer is never handed to the save path');
  host.close();
});

test('polling_resumes_when_the_tab_is_shown', async () => {
  const { frame, appear } = standInFrame();
  const { handlers } = recorder();
  const editor = standInEditor({ 'document.export': exported(2, true) });
  appear(editor.solidcraft);
  frame.ownerDocument.visibilityState = 'hidden';
  const host = connect(frame, FAST, handlers);
  await wait(50);
  assert.equal(editor.count('document.export'), 0);
  frame.ownerDocument.visibilityState = 'visible';
  await wait(50);
  assert.ok(editor.count('document.export') >= 1, 'polled again once shown');
  host.close();
});

test('a_retried_save_still_confirms', async () => {
  const { frame, appear } = standInFrame();
  const { seen, handlers } = recorder();
  const editor = standInEditor({
    'document.export': exported(11),
    'document.saved': (p) => ({ revision: p.revision, dirty: false }),
  });
  appear(editor.solidcraft);
  const host = connect(frame, { ...FAST, pollEvery: 100000 }, handlers);
  await wait(20);
  host.requestSave();
  await wait(20);
  // The first upload of these bytes failed; the queue sends the same bytes object again,
  // and that one is stored.
  const bytes = seen.save[0].bytes;
  await wait(20);
  host.stored(bytes);
  await wait(20);
  assert.deepEqual(
    editor.calls.filter((c) => c.method === 'document.saved').map((c) => c.params),
    [{ revision: 11 }],
  );
  host.close();
});

test('only_the_two_newest_exports_are_kept', async () => {
  const { frame, appear } = standInFrame();
  const { seen, handlers } = recorder();
  let revision = 0;
  const editor = standInEditor({
    'document.export': () => exported(++revision),
    'document.saved': (p) => ({ revision: p.revision, dirty: false }),
  });
  appear(editor.solidcraft);
  const host = connect(frame, { ...FAST, pollEvery: 100000 }, handlers);
  await wait(20);
  for (let i = 0; i < 3; i++) {
    host.requestSave();
    await wait(10);
  }
  assert.equal(seen.save.length, 3);
  host.stored(seen.save[0].bytes); // dropped: older than the two kept
  await wait(20);
  assert.equal(editor.count('document.saved'), 0);
  host.stored(seen.save[2].bytes);
  await wait(20);
  assert.deepEqual(
    editor.calls.filter((c) => c.method === 'document.saved').map((c) => c.params.revision),
    [3],
  );
  host.close();
});
