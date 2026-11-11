import test from 'node:test';
import assert from 'node:assert/strict';
import {
  BRIDGES,
  SaveQueue,
  connect,
  classifySave,
  editorUrl,
  nextStatus,
  noticeAfter,
  parseHostMessage,
  saveFailure,
  seatBanner,
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
  assert.deepEqual(Object.keys(fromView).sort(), ['bytes', 'kind', 'name'], 'nothing unused is carried');
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

// ---- the banner and notices ----

const ME = '6f1c1a52-6a43-4b8e-9d0e-3c3f6c1d2b10';
const OTHER = '0b7d9a4e-1111-4c2a-8f00-5a5a5a5a5a5a';
const page = (over = {}) => ({
  joined: true,
  holder: false,
  stopped: false,
  canEdit: true,
  seat: { state: 'free' },
  me: ME,
  mine: false,
  ...over,
});
const taken = (user, name = 'Bob') => ({ state: 'taken', by: { name, user } });

test('the_banner_follows_the_seat_and_the_right_to_edit', () => {
  // Nothing to say before the first join, for the holder, or once the page said it stopped.
  for (const over of [{ joined: false }, { holder: true }, { stopped: true }]) {
    assert.equal(seatBanner(page(over)), null, JSON.stringify(over));
  }
  assert.deepEqual(seatBanner(page({ canEdit: false })), {
    text: 'View only. Changes you make here are not saved.',
    actions: [],
  });
  // Given the right to edit while the seat is free: offered a reload to take it.
  assert.deepEqual(seatBanner(page()), { text: 'This file is free to edit. Reload to edit it.', actions: ['reload'] });
  // Given it while someone edits: told who.
  const bob = seatBanner(page({ seat: taken(OTHER) }));
  assert.match(bob.text, /^Bob is editing this file\./);
  assert.deepEqual(bob.actions, []);
  assert.match(seatBanner(page({ seat: taken(OTHER, '') })).text, /^Someone is editing/);
  // The right to edit taken away: view only, whatever the seat.
  assert.match(seatBanner(page({ canEdit: false, seat: taken(OTHER) })).text, /^View only/);
  assert.match(seatBanner(page({ seat: { state: 'unknown' } })).text, /^View only/);
});

test('a_kept_copy_is_offered_on_every_banner', () => {
  for (const over of [{ canEdit: false }, { seat: taken(OTHER) }, { seat: taken(ME) }, { seat: { state: 'free' } }]) {
    const banner = seatBanner(page({ ...over, mine: true }));
    assert.ok(banner.actions.includes('mycopy'), JSON.stringify(over));
    assert.ok(!seatBanner(page(over)).actions.includes('mycopy'), JSON.stringify(over));
  }
  assert.deepEqual(seatBanner(page({ mine: true })).actions, ['mycopy', 'reload']);
});

const LOST = 'You can no longer edit this file.';
const LOST_WITH_COPY =
  'You can no longer edit this file, so changes you make here are not saved. Download your copy, then reload.';

test('a_demoted_holder_is_told_what_it_costs', () => {
  const demoted = (over) => page({ stopped: true, lostEdit: true, canEdit: false, ...over });
  // Nothing to lose: no copy is offered, only a reload.
  assert.deepEqual(seatBanner(demoted()), { text: LOST, actions: ['reload'] });
  // Unsaved changes in the editor, or a copy the page keeps: the copy comes first.
  for (const over of [{ unsaved: true }, { mine: true }]) {
    assert.deepEqual(seatBanner(demoted(over)), { text: LOST_WITH_COPY, actions: ['mycopy', 'reload'] });
  }
  // A page that stopped for another reason says so itself.
  assert.equal(seatBanner(page({ stopped: true, canEdit: false })), null);
  assert.equal(seatBanner(page({ stopped: true })), null);
});

test('a_holder_given_the_right_back_is_offered_the_seat', () => {
  // Holding the seat: nothing to say. Demoted: told so.
  assert.equal(seatBanner(page({ holder: true })), null);
  const lost = { stopped: true, lostEdit: true };
  assert.equal(seatBanner(page({ ...lost, canEdit: false })).text, LOST);
  // Promoted again while the seat is free: a reload takes it.
  assert.deepEqual(seatBanner(page({ ...lost, canEdit: true })), {
    text: 'This file is free to edit. Reload to edit it.',
    actions: ['reload'],
  });
  assert.deepEqual(seatBanner(page({ ...lost, canEdit: true, unsaved: true })).actions, ['mycopy', 'reload']);
  // Promoted again while someone else took it: told who.
  const bob = seatBanner(page({ ...lost, canEdit: true, seat: taken(OTHER) }));
  assert.match(bob.text, /^Bob is editing this file\./);
  assert.deepEqual(bob.actions, []);
  assert.deepEqual(seatBanner(page({ ...lost, canEdit: true, seat: taken(OTHER), mine: true })).actions, ['mycopy']);
});

test('a_second_tab_of_the_holder_is_told_so', () => {
  const own = seatBanner(page({ seat: taken(ME, 'Ann') }));
  assert.equal(
    own.text,
    'This file is open for editing in another tab or window of yours. Changes you make here are not saved.',
  );
  // Someone else with the same name is someone else.
  assert.match(seatBanner(page({ seat: taken(OTHER, 'Ann') })).text, /^Ann is editing/);
  // Without a known account id nobody is taken for the signed-in person.
  assert.match(seatBanner(page({ me: '', seat: taken('', 'Ann') })).text, /^Ann is editing/);
});

test('not_answering_keeps_a_stop_or_save_failure', () => {
  const late = { kind: 'not_answering', text: 'The editor did not answer. Show this tab and try again.' };
  for (const current of [
    { kind: 'stopped', text: 'Someone else is editing this file now.', actions: ['mycopy', 'reload'] },
    { kind: 'save', text: 'Could not reach the server.', actions: ['retry', 'mycopy'] },
  ]) {
    const shown = noticeAfter(current, late);
    assert.equal(shown.kind, current.kind);
    assert.deepEqual(shown.actions, current.actions, 'its actions stay');
    assert.equal(shown.text, `${current.text} ${late.text}`);
    assert.equal(noticeAfter(shown, late), shown, 'said once');
  }
  // Nothing showing, or a notice without actions to keep: as before.
  assert.equal(noticeAfter(null, late), late);
  assert.equal(noticeAfter({ kind: 'newer', text: 'A newer version was saved.', actions: ['reload'] }, late), late);
  assert.equal(noticeAfter({ kind: 'not_answering', text: late.text }, late), late);
});

// ---- the craft-web-host bridge, with a stand-in editor ----

// A stand-in frame whose window may hold `<prefix>.execute`, recording what is posted to it.
function craftFrame(prefix, app) {
  const posted = [];
  const contentWindow = { postMessage: (message) => posted.push(message) };
  if (app) contentWindow[prefix] = app;
  const defaultView = {
    location: { origin: 'https://sharecraft.test' },
    addEventListener() {},
    removeEventListener() {},
  };
  return { frame: { contentWindow, ownerDocument: { defaultView } }, posted };
}

function recordingEditor(answer) {
  const executed = [];
  return {
    executed,
    app: {
      execute(command, params) {
        executed.push({ command, params });
        return answer;
      },
    },
  };
}

test('a_new_empty_document_asks_for_a_blank_one', () => {
  assert.equal(BRIDGES.wordcraft.blank, 'file.new');
  assert.equal(BRIDGES.gridcraft.blank, undefined, 'GridCraft starts with an empty workbook');
  assert.equal(BRIDGES.pdfcraft.blank, undefined, 'a PDF cannot be made empty');

  // Never saved: no bytes, so the editor is asked for a blank document and nothing is opened.
  const empty = recordingEditor(undefined);
  const fresh = craftFrame('wordcraft', empty.app);
  connect(fresh.frame, BRIDGES.wordcraft, {}).start('Letter.docx', null);
  assert.deepEqual(empty.executed, [{ command: 'file.new', params: {} }]);
  assert.deepEqual(fresh.posted, []);

  // Saved before: the bytes are opened and no blank document is made.
  const stored = recordingEditor(undefined);
  const saved = craftFrame('wordcraft', stored.app);
  const bytes = new Uint8Array([1, 2, 3]);
  connect(saved.frame, BRIDGES.wordcraft, {}).start('Letter.docx', bytes);
  assert.deepEqual(stored.executed, []);
  assert.equal(saved.posted.length, 1);
  assert.equal(saved.posted[0].type, 'wordcraft:open');
  assert.equal(saved.posted[0].bytes, bytes);

  // An editor without a blank command: neither.
  const grid = recordingEditor(undefined);
  const sheet = craftFrame('gridcraft', grid.app);
  connect(sheet.frame, BRIDGES.gridcraft, {}).start('Book.xlsx', null);
  assert.deepEqual(grid.executed, []);
  assert.deepEqual(sheet.posted, []);
});

// A stand-in WordCraft window with a canvas that records the key events sent to it, and an
// editor whose commands answer with a promise. `log` keeps the order of both.
function wordcraftWindow({ canvas = true } = {}) {
  const log = [];
  class KeyboardEvent {
    constructor(type, init) {
      this.type = type;
      Object.assign(this, init);
    }
  }
  const element = {
    dispatchEvent(event) {
      log.push(`${event.type}:${event.key}:${event.keyCode}:${event.bubbles}`);
      return true;
    },
  };
  const app = {
    execute(command) {
      return new Promise((resolve) =>
        setTimeout(() => {
          log.push(`execute:${command}`);
          resolve(true);
        }, 5),
      );
    },
  };
  const { frame, posted } = craftFrame('wordcraft', app);
  frame.contentWindow.KeyboardEvent = KeyboardEvent;
  frame.contentWindow.document = { querySelector: (selector) => (selector === 'canvas' && canvas ? element : null) };
  return { frame, posted, log };
}

test('the_start_screen_is_dismissed_after_the_blank_document', async () => {
  assert.equal(BRIDGES.wordcraft.dismiss, 'Escape');
  assert.equal(BRIDGES.gridcraft.dismiss, undefined);
  const quick = { ...BRIDGES.wordcraft, dismissAfter: 5 };

  // Blank first, then Escape pressed and released on the editor's canvas.
  const blank = wordcraftWindow();
  await connect(blank.frame, quick, {}).start('Letter.docx', null);
  assert.deepEqual(blank.log, ['execute:file.new', 'keydown:Escape:27:true', 'keyup:Escape:27:true']);

  // Bytes opened: no blank document, so nothing is dismissed.
  const opened = wordcraftWindow();
  await connect(opened.frame, quick, {}).start('Letter.docx', new Uint8Array([1]));
  await new Promise((resolve) => setTimeout(resolve, 20));
  assert.deepEqual(opened.log, []);
  assert.equal(opened.posted.length, 1);

  // No canvas: nothing is sent and nothing is thrown.
  const bare = wordcraftWindow({ canvas: false });
  await connect(bare.frame, quick, {}).start('Letter.docx', null);
  assert.deepEqual(bare.log, ['execute:file.new']);

  // A document that cannot be read is no failure either.
  const locked = wordcraftWindow();
  Object.defineProperty(locked.frame.contentWindow, 'document', {
    get() {
      throw new Error('SecurityError');
    },
  });
  await connect(locked.frame, quick, {}).start('Letter.docx', null);
  assert.deepEqual(locked.log, ['execute:file.new']);
});

test('a_save_request_needs_the_editor_interface', () => {
  const ask = (frame) => connect(frame, BRIDGES.gridcraft, {}).requestSave();
  // No interface, an interface without `execute`, or a window that cannot be read: not asked.
  assert.equal(ask(craftFrame('gridcraft', null).frame), false);
  assert.equal(ask(craftFrame('gridcraft', { execute: 'no' }).frame), false);
  const throwing = craftFrame('gridcraft', null).frame;
  Object.defineProperty(throwing, 'contentWindow', {
    get() {
      throw new Error('SecurityError');
    },
  });
  assert.equal(ask(throwing), false);
  // The editor refused.
  assert.equal(ask(craftFrame('gridcraft', recordingEditor(false).app).frame), false);
  // Asked: the editor answers nothing in particular, or true.
  const editor = recordingEditor(undefined);
  assert.equal(ask(craftFrame('gridcraft', editor.app).frame), true);
  assert.deepEqual(editor.executed, [{ command: 'file.save', params: {} }]);
  assert.equal(ask(craftFrame('gridcraft', recordingEditor(true).app).frame), true);
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
  // Wired as the page wires it: saves go to a queue, and a stored save is reported back.
  const { calls, upload } = standIn();
  const events = [];
  let host = null;
  const queue = new SaveQueue(upload, (event) => {
    events.push(event.type);
    if (event.type === 'stored') host.stored(event.bytes);
  });
  host = connect(frame, { ...FAST, pollEvery: 100000 }, { ...handlers, save: ({ bytes }) => queue.offer(bytes) });
  await wait(20);
  host.requestSave();
  await wait(20);
  assert.equal(calls.length, 1, 'the export was uploaded');
  // The first upload fails: the editor is not told it is saved.
  calls[0].answer({ ok: false, status: 503 });
  await wait(20);
  assert.equal(editor.count('document.saved'), 0);
  // The retry sends the same bytes and is stored: now it is told, with that export's revision.
  queue.retry();
  assert.equal(calls.length, 2);
  assert.equal(calls[1].bytes, calls[0].bytes, 'the same bytes are sent again');
  calls[1].answer({ ok: true });
  await wait(20);
  assert.deepEqual(events, ['saving', 'not_stored', 'saving', 'stored']);
  assert.deepEqual(
    editor.calls.filter((c) => c.method === 'document.saved').map((c) => c.params),
    [{ revision: 11 }],
  );
  assert.equal(seen.save.length, 0, 'every save went to the queue');
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
