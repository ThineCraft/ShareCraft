import test from 'node:test';
import assert from 'node:assert/strict';
import {
  BRIDGES,
  SaveQueue,
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
  assert.equal(editorUrl('solidcraft', 'A'), '/editors/solidcraft/index.html?embed=editor&author=A');
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
