import test from 'node:test';
import assert from 'node:assert/strict';
import {
  ANNOUNCED_STEPS,
  TRAY_STATES,
  announceStep,
  countStates,
  createRateMeter,
  createUploads,
  eventFor,
  isActive,
  nextStatus,
  overallPercent,
  rowActions,
  rowStats,
  speedText,
  timeLeftText,
  trayHeader,
} from './upload-tray.js';

const MB = 1024 * 1024;

// ---- the state machine ----

test('a file moves queued → running → done, and only along allowed paths', () => {
  assert.deepEqual(TRAY_STATES, ['queued', 'running', 'paused', 'conflict', 'done', 'skipped', 'failed', 'cancelled']);
  assert.equal(nextStatus('queued', 'start'), 'running');
  assert.equal(nextStatus('running', 'progress'), 'running');
  assert.equal(nextStatus('running', 'finish'), 'done');
  assert.equal(nextStatus('queued', 'finish'), null);
  assert.equal(nextStatus('done', 'cancel'), null);
  assert.equal(nextStatus('cancelled', 'resume'), null);
  assert.equal(nextStatus('nonsense', 'start'), null);
});

test('pause, resume and cancel', () => {
  assert.equal(nextStatus('queued', 'pause'), 'paused');
  assert.equal(nextStatus('running', 'pause'), 'paused');
  assert.equal(nextStatus('paused', 'resume'), 'queued');
  assert.equal(nextStatus('paused', 'cancel'), 'cancelled');
  assert.equal(nextStatus('running', 'cancel'), 'cancelled');
  assert.equal(nextStatus('paused', 'progress'), null, 'a late chunk does not undo a pause');
});

test('a name conflict waits for an answer, then runs on, is skipped or cancelled', () => {
  assert.equal(nextStatus('running', 'conflict'), 'conflict');
  assert.equal(nextStatus('conflict', 'answer'), 'running');
  assert.equal(nextStatus('conflict', 'skip'), 'skipped');
  assert.equal(nextStatus('conflict', 'cancel'), 'cancelled');
  assert.equal(nextStatus('queued', 'conflict'), null);
});

test('a failed file can be retried; finished ones stay finished', () => {
  assert.equal(nextStatus('running', 'fail'), 'failed');
  assert.equal(nextStatus('failed', 'retry'), 'queued');
  assert.equal(nextStatus('done', 'retry'), null);
  assert.equal(nextStatus('skipped', 'retry'), null);
});

test('only waiting, running, paused and conflicting files are still to do', () => {
  assert.deepEqual(TRAY_STATES.filter(isActive), ['queued', 'running', 'paused', 'conflict']);
});

test('what the upload queue reports maps onto the events', () => {
  assert.equal(eventFor('waiting'), null);
  assert.equal(eventFor('hashing'), 'progress');
  assert.equal(eventFor('uploading'), 'progress');
  assert.equal(eventFor('conflict'), 'conflict');
  assert.equal(eventFor('done'), 'finish');
  assert.equal(eventFor('skipped'), 'skip');
  assert.equal(eventFor('failed'), 'fail');
  assert.equal(eventFor('paused'), 'pause');
  assert.equal(eventFor('cancelled'), 'cancel');
});

test('each state offers its own buttons', () => {
  assert.deepEqual(rowActions('queued'), ['pause', 'cancel']);
  assert.deepEqual(rowActions('running'), ['pause', 'cancel']);
  assert.deepEqual(rowActions('paused'), ['resume', 'cancel']);
  assert.deepEqual(rowActions('conflict'), ['cancel']);
  assert.deepEqual(rowActions('done', { open: true }), ['open', 'remove']);
  assert.deepEqual(rowActions('done'), ['remove']);
  assert.deepEqual(rowActions('failed', { retry: true }), ['retry', 'remove']);
  assert.deepEqual(rowActions('failed'), ['remove']);
  assert.deepEqual(rowActions('cancelled'), ['remove']);
});

// ---- speed and time left ----

function fakeClock(start = 0) {
  let at = start;
  return { now: () => at, tick: (ms) => (at += ms) };
}

test('speed is unknown until a second has been measured', () => {
  const clock = fakeClock();
  const meter = createRateMeter({ now: clock.now });
  meter.sample(0);
  assert.equal(meter.speed(), null);
  clock.tick(500);
  meter.sample(MB / 2);
  assert.equal(meter.speed(), null);
  assert.equal(meter.timeLeft(10 * MB), null);
  clock.tick(500);
  meter.sample(MB);
  assert.equal(Math.round(meter.speed()), MB);
  assert.equal(Math.round(meter.timeLeft(10 * MB)), 10);
});

test('one slow chunk moves the estimate only part of the way', () => {
  const clock = fakeClock();
  const meter = createRateMeter({ now: clock.now, halfLife: 4000 });
  let sent = 0;
  meter.sample(sent);
  for (let i = 0; i < 10; i += 1) {
    clock.tick(1000);
    sent += 8 * MB; // 8 MB/s
    meter.sample(sent);
  }
  assert.equal(Math.round(meter.speed() / MB), 8);
  clock.tick(1000);
  sent += 1 * MB; // one chunk at 1 MB/s
  meter.sample(sent);
  const speed = meter.speed() / MB;
  assert.ok(speed > 6 && speed < 8, `smoothed: ${speed}`);
});

test('a long gap halves the old speed’s weight after the half-life', () => {
  const clock = fakeClock();
  const meter = createRateMeter({ now: clock.now, halfLife: 2000, warmup: 0 });
  meter.sample(0);
  clock.tick(1000);
  meter.sample(10 * MB); // 10 MB/s
  clock.tick(2000);
  meter.sample(10 * MB + 4 * MB); // 2 MB/s over one half-life
  assert.equal(Math.round(meter.speed() / MB), 6);
});

test('a pause is not counted as slow: measuring starts again from where it stands', () => {
  const clock = fakeClock();
  const meter = createRateMeter({ now: clock.now });
  meter.sample(0);
  clock.tick(2000);
  meter.sample(4 * MB); // 2 MB/s
  clock.tick(60_000); // paused for a minute
  meter.rebase(4 * MB);
  clock.tick(1000);
  meter.sample(6 * MB);
  assert.equal(Math.round(meter.speed() / MB), 2);
});

test('a total that goes down (a restart) starts measuring again', () => {
  const clock = fakeClock();
  const meter = createRateMeter({ now: clock.now, warmup: 0 });
  meter.sample(0);
  clock.tick(1000);
  meter.sample(3 * MB);
  clock.tick(1000);
  meter.sample(0);
  assert.equal(Math.round(meter.speed() / MB), 3);
});

test('time left is said in rounded, plain words', () => {
  assert.equal(timeLeftText(null), '');
  assert.equal(timeLeftText(Infinity), '');
  assert.equal(timeLeftText(3), 'a few seconds left');
  assert.equal(timeLeftText(45), 'less than a minute left');
  assert.equal(timeLeftText(65), 'about 1 minute left');
  assert.equal(timeLeftText(130), 'about 2 minutes left');
  assert.equal(timeLeftText(1260), 'about 20 minutes left');
  assert.equal(timeLeftText(1380), 'about 25 minutes left');
  assert.equal(timeLeftText(3500), 'about 1 hour left');
  assert.equal(timeLeftText(3600 + 1200), 'about 1 hour 20 minutes left');
  assert.equal(timeLeftText(2 * 3600 + 100), 'about 2 hours left');
  assert.equal(timeLeftText(3600 + 3500), 'about 2 hours left');
  assert.equal(timeLeftText(7 * 3600 + 1800), 'about 7 hours left');
});

test('speed is shown per second', () => {
  assert.equal(speedText(null), '');
  assert.equal(speedText(1.5 * MB), '1.5 MB/s');
});

// ---- the header ----

const docs = { id: 'lib', name: 'Documents' };
const entry = (status, size = 10, sent = 0, destination = docs) => ({ status, size, sent, destination });

test('while uploads run, the header says which file of how many and how long is left', () => {
  const entries = [entry('done'), entry('done'), entry('running', 10, 4), entry('queued'), entry('queued')];
  assert.equal(trayHeader({ entries, secondsLeft: 130 }).title, 'Uploading 3 of 5 — about 2 minutes left');
  assert.equal(trayHeader({ entries }).title, 'Uploading 3 of 5');
  assert.equal(trayHeader({ entries }).finished, false);
});

test('a question in a row is put before the time left', () => {
  const entries = [entry('conflict'), entry('queued')];
  assert.equal(trayHeader({ entries, secondsLeft: 30 }).title, 'Uploading 1 of 2 — 1 file needs an answer');
});

test('when everything left is paused, the header says so', () => {
  const entries = [entry('done'), entry('paused'), entry('paused')];
  assert.equal(trayHeader({ entries }).title, 'Paused — 1 of 3 uploaded');
});

test('cancelled files are not counted in "of"', () => {
  const entries = [entry('cancelled'), entry('running'), entry('queued')];
  assert.equal(trayHeader({ entries }).title, 'Uploading 1 of 2');
});

test('when all have finished, the header sums up where they went', () => {
  assert.deepEqual(trayHeader({ entries: [entry('done'), entry('done'), entry('done'), entry('done'), entry('done')] }), {
    title: '5 files uploaded to Documents',
    finished: true,
    failed: 0,
  });
  assert.equal(trayHeader({ entries: [entry('done')] }).title, '1 file uploaded to Documents');
  const two = [entry('done'), entry('done', 10, 0, { id: 'f', name: 'Photos' })];
  assert.equal(trayHeader({ entries: two }).title, '2 files uploaded');
  assert.equal(trayHeader({ entries: [entry('done'), entry('skipped')] }).title, '1 file uploaded to Documents (1 skipped)');
  assert.equal(trayHeader({ entries: [entry('done'), entry('cancelled')] }).title, '1 file uploaded to Documents (1 cancelled)');
});

test('failures are put first in the summary', () => {
  const result = trayHeader({ entries: [entry('done'), entry('done'), entry('failed')] });
  assert.equal(result.title, '2 of 3 uploaded — 1 failed');
  assert.equal(result.failed, 1);
  assert.equal(trayHeader({ entries: [entry('skipped'), entry('skipped')] }).title, 'No files uploaded — 2 skipped');
  assert.equal(trayHeader({ entries: [entry('cancelled')] }).title, 'Uploads cancelled');
  assert.equal(trayHeader({ entries: [] }).title, '');
});

test('the overall percentage counts bytes, leaves cancelled files out and counts skipped ones as done', () => {
  assert.equal(overallPercent([entry('running', 100, 50), entry('queued', 100)]), 25);
  assert.equal(overallPercent([entry('done', 100, 0), entry('running', 100, 50)]), 75);
  assert.equal(overallPercent([entry('cancelled', 100), entry('running', 100, 50)]), 50);
  assert.equal(overallPercent([entry('skipped', 100), entry('running', 100, 0)]), 50);
  assert.equal(overallPercent([]), 100);
  assert.deepEqual(countStates([entry('done'), entry('done'), entry('failed')]).done, 2);
});

test('progress is announced at 25, 50, 75 and 100 %, each once', () => {
  assert.deepEqual(ANNOUNCED_STEPS, [25, 50, 75, 100]);
  let last = 0;
  const said = [];
  for (const percent of [0, 3, 10, 24, 25, 26, 49, 51, 60, 74, 80, 99, 100, 100]) {
    const step = announceStep(last, percent);
    last = step.step;
    if (step.say) said.push(percent);
  }
  assert.deepEqual(said, [25, 51, 80, 100]);
});

test('when more files are added and the percentage falls, the steps come round again', () => {
  let step = announceStep(50, 30);
  assert.deepEqual(step, { step: 25, say: false });
  step = announceStep(step.step, 52);
  assert.deepEqual(step, { step: 50, say: true });
});

test('a row says bytes, speed and time left while it runs', () => {
  const running = { status: 'running', size: 20 * MB, sent: 3 * MB, phase: 'sending' };
  assert.equal(rowStats(running, { speed: 1.2 * MB, secondsLeft: 65 }), '3.0 MB of 20.0 MB · 1.2 MB/s · about 1 minute left');
  assert.equal(rowStats(running), '3.0 MB of 20.0 MB');
  assert.equal(rowStats({ ...running, phase: 'hashing', hashed: 8 * MB }), 'Checking the file… 8.0 MB of 20.0 MB');
  assert.equal(rowStats({ status: 'paused', size: 20 * MB, sent: 8 * MB }), '8.0 MB of 20.0 MB · paused');
  assert.equal(rowStats({ status: 'queued', size: 20 * MB, sent: 0 }), '20.0 MB · waiting');
});

// ---- the controller, with a stubbed upload queue ----

/** A queue whose runs the test finishes by hand. */
function stubQueues() {
  const made = [];
  const makeQueue = (options) => {
    const queue = {
      options,
      runs: [],
      halted: [],
      runOne(outcome) {
        return new Promise((resolve) => {
          queue.runs.push({ outcome, end: (fields) => resolve(Object.assign(outcome, fields)) });
        });
      },
      halt(outcome, kind) {
        queue.halted.push(kind);
        outcome.halt = kind;
      },
    };
    made.push(queue);
    return queue;
  };
  return { made, makeQueue };
}

const fileOf = (name, size = 10) => ({ name, size, path: [], slice: () => null });
const flush = () => new Promise((resolve) => setImmediate(resolve));

test('files run one after another and the tray is told when all have stopped', async () => {
  const { made, makeQueue } = stubQueues();
  let idle = 0;
  const uploads = createUploads({ makeQueue, onIdle: () => (idle += 1) });
  const [a, b] = uploads.add([fileOf('a.pdf'), fileOf('b.pdf')], docs);
  assert.equal(a.status, 'running');
  assert.equal(b.status, 'queued');
  assert.equal(uploads.active(), 2);
  made[0].runs[0].end({ status: 'done', docId: 'd1', editor: 'pdf' });
  await flush();
  assert.equal(a.status, 'done');
  assert.equal(a.docId, 'd1');
  assert.equal(b.status, 'running');
  made[0].runs[1].end({ status: 'done' });
  await flush();
  assert.equal(uploads.active(), 0);
  assert.equal(idle, 1);
  assert.equal(trayHeader({ entries: uploads.entries }).title, '2 files uploaded to Documents');
});

test('progress from the queue moves the bytes, and a late chunk does not undo a pause', async () => {
  const { made, makeQueue } = stubQueues();
  const uploads = createUploads({ makeQueue });
  const [a] = uploads.add([fileOf('a.pdf', 100)], docs);
  const outcome = made[0].runs[0].outcome;
  made[0].options.onChange(Object.assign(outcome, { status: 'uploading', sent: 40 }));
  assert.equal(a.sent, 40);
  uploads.pause(a.id);
  assert.equal(a.status, 'paused');
  assert.deepEqual(made[0].halted, ['pause']);
  made[0].options.onChange(Object.assign(outcome, { status: 'uploading', sent: 60 }));
  assert.equal(a.status, 'paused');
  made[0].runs[0].end({ status: 'paused', keep: true, sessionId: 's1' });
  await flush();
  assert.equal(a.status, 'paused');
  assert.equal(a.keep, true);
});

test('a paused file resumes from its kept session', async () => {
  const { made, makeQueue } = stubQueues();
  const uploads = createUploads({ makeQueue });
  const [a] = uploads.add([fileOf('a.pdf', 100)], docs);
  made[0].runs[0].outcome.parentId = 'lib';
  uploads.pause(a.id);
  made[0].runs[0].end({ status: 'paused', keep: true, sessionId: 's1', sent: 60 });
  await flush();
  uploads.resume(a.id);
  await flush();
  assert.equal(a.status, 'running');
  const second = made[0].runs[1].outcome;
  assert.equal(second.file.resumeId, 's1');
  assert.equal(second.file.parentId, 'lib');
});

test('resuming before a pause has landed puts the file back in the queue', async () => {
  const { made, makeQueue } = stubQueues();
  const uploads = createUploads({ makeQueue });
  const [a] = uploads.add([fileOf('a.pdf', 100)], docs);
  uploads.pause(a.id);
  uploads.resume(a.id);
  made[0].runs[0].end({ status: 'paused', keep: true, sessionId: 's1' });
  await flush();
  assert.equal(a.status, 'running');
  assert.equal(made[0].runs[1].outcome.file.resumeId, 's1');
});

test('a waiting file can be paused and is passed over until resumed', async () => {
  const { made, makeQueue } = stubQueues();
  const uploads = createUploads({ makeQueue });
  const [a, b, c] = uploads.add([fileOf('a.pdf'), fileOf('b.pdf'), fileOf('c.pdf')], docs);
  uploads.pause(b.id);
  made[0].runs[0].end({ status: 'done' });
  await flush();
  assert.equal(a.status, 'done');
  assert.equal(b.status, 'paused');
  assert.equal(c.status, 'running');
});

test('cancelling a running file halts it; cancelling a paused one gives its session back', async () => {
  const released = [];
  const { made, makeQueue } = stubQueues();
  const uploads = createUploads({ makeQueue, release: (id) => released.push(id) });
  const [a, b] = uploads.add([fileOf('a.pdf'), fileOf('b.pdf')], docs);
  uploads.cancel(a.id);
  assert.equal(a.status, 'cancelled');
  assert.deepEqual(made[0].halted, ['cancel']);
  made[0].runs[0].end({ status: 'cancelled' });
  await flush();
  assert.equal(b.status, 'running');
  uploads.pause(b.id);
  made[0].runs[1].end({ status: 'paused', keep: true, sessionId: 's2' });
  await flush();
  uploads.cancel(b.id);
  assert.equal(b.status, 'cancelled');
  assert.deepEqual(released, ['s2']);
});

test('pausing then cancelling before the pause lands gives the session back', async () => {
  const released = [];
  const { made, makeQueue } = stubQueues();
  const uploads = createUploads({ makeQueue, release: (id) => released.push(id) });
  const [a] = uploads.add([fileOf('a.pdf')], docs);
  uploads.pause(a.id);
  uploads.cancel(a.id);
  made[0].runs[0].end({ status: 'paused', keep: true, sessionId: 's1' });
  await flush();
  assert.equal(a.status, 'cancelled');
  assert.deepEqual(released, ['s1']);
});

test('a name conflict is answered in the row, and "Apply to all" answers the next one too', async () => {
  const { made, makeQueue } = stubQueues();
  const uploads = createUploads({ makeQueue });
  const [a, b] = uploads.add([fileOf('a.pdf'), fileOf('b.pdf')], docs);
  const { options } = made[0];
  const first = made[0].runs[0].outcome;
  options.onChange(Object.assign(first, { status: 'conflict', suggested: 'a (2).pdf' }));
  assert.equal(a.status, 'conflict');
  assert.equal(a.suggested, 'a (2).pdf');
  const answered = options.resolve({ name: 'a.pdf', suggested: 'a (2).pdf', outcome: first });
  uploads.answer(a.id, 'keep_both', true);
  assert.deepEqual(await answered, { choice: 'keep_both', all: false });
  assert.equal(a.status, 'running');
  made[0].runs[0].end({ status: 'done' });
  await flush();
  const second = made[0].runs[1].outcome;
  assert.equal(b.status, 'running');
  assert.deepEqual(await options.resolve({ name: 'b.pdf', suggested: 'b (2).pdf', outcome: second }), { choice: 'keep_both', all: false });
});

test('a failed file is retried from its kept session, or afresh', async () => {
  const { made, makeQueue } = stubQueues();
  const uploads = createUploads({ makeQueue });
  const [a] = uploads.add([fileOf('a.pdf')], docs);
  made[0].runs[0].end({ status: 'failed', retry: true, keep: true, sessionId: 's1', message: 'The connection was lost.' });
  await flush();
  assert.equal(a.status, 'failed');
  assert.equal(a.message, 'The connection was lost.');
  uploads.retry(a.id);
  assert.equal(a.status, 'running');
  assert.equal(made[0].runs[1].outcome.file.resumeId, 's1');
  made[0].runs[1].end({ status: 'failed', retry: false, message: 'Too big.' });
  await flush();
  uploads.retry(a.id);
  assert.equal(a.status, 'failed', 'not retried when the failure cannot be helped');
});

test('stopped files can be removed; running ones cannot', async () => {
  const released = [];
  const { made, makeQueue } = stubQueues();
  const uploads = createUploads({ makeQueue, release: (id) => released.push(id) });
  const [a, b] = uploads.add([fileOf('a.pdf'), fileOf('b.pdf')], docs);
  uploads.remove(a.id);
  assert.equal(uploads.entries.length, 2);
  made[0].runs[0].end({ status: 'failed', retry: true, keep: true, sessionId: 's1' });
  await flush();
  uploads.remove(a.id);
  assert.deepEqual(released, ['s1']);
  assert.deepEqual(uploads.entries, [b]);
});

test('cancel all stops everything still to do, and clear empties the tray', async () => {
  const { made, makeQueue } = stubQueues();
  const uploads = createUploads({ makeQueue });
  uploads.add([fileOf('a.pdf'), fileOf('b.pdf'), fileOf('c.pdf')], docs);
  uploads.cancelAll();
  assert.equal(uploads.active(), 0);
  made[0].runs[0].end({ status: 'cancelled' });
  await flush();
  assert.equal(made[0].runs.length, 1, 'nothing else was started');
  uploads.clear();
  assert.equal(uploads.entries.length, 0);
});

test('uploads added later for another folder keep their own destination', () => {
  const { made, makeQueue } = stubQueues();
  const uploads = createUploads({ makeQueue });
  uploads.add([fileOf('a.pdf')], docs);
  const [b] = uploads.add([fileOf('b.pdf')], { id: 'f1', name: 'Photos' });
  assert.equal(made.length, 2);
  assert.equal(made[1].options.parent, 'f1');
  assert.equal(b.destination.name, 'Photos');
});

test('the time left comes from the measured speed once bytes are moving', async () => {
  const clock = fakeClock();
  const { made, makeQueue } = stubQueues();
  const uploads = createUploads({ makeQueue, now: clock.now });
  const [a] = uploads.add([fileOf('a.pdf', 100 * MB), fileOf('b.pdf', 100 * MB)], docs);
  const outcome = made[0].runs[0].outcome;
  const change = (fields) => made[0].options.onChange(Object.assign(outcome, fields));
  change({ status: 'hashing', hashed: 50 * MB });
  assert.equal(uploads.secondsLeft(), null);
  clock.tick(5000); // checking the file is not counted
  change({ status: 'uploading', sent: 0 });
  for (let i = 1; i <= 4; i += 1) {
    clock.tick(1000);
    change({ status: 'uploading', sent: i * 8 * MB });
  }
  assert.equal(Math.round(uploads.speed() / MB), 8);
  assert.equal(Math.round(uploads.secondsLeft()), Math.round((200 - 32) / 8));
  const pace = uploads.pace(a);
  assert.equal(Math.round(pace.secondsLeft), Math.round((100 - 32) / 8));
});
