import test from 'node:test';
import assert from 'node:assert/strict';
import { RETRY_STEPS, bannerModel, createConnection, retryDelay, retrySchedule } from './connection.js';

/** Timers the test runs by hand. */
function timers() {
  let t = 0;
  let id = 0;
  const due = new Map();
  return {
    now: () => t,
    setTimer: (fn, ms) => {
      id += 1;
      due.set(id, { at: t + ms, fn });
      return id;
    },
    clearTimer: (which) => due.delete(which),
    /** Moves the clock by `ms`, running what falls due. */
    async advance(ms) {
      t += ms;
      for (const [which, timer] of [...due].sort((a, b) => a[1].at - b[1].at)) {
        if (timer.at <= t && due.has(which)) {
          due.delete(which);
          timer.fn();
          await Promise.resolve();
          await Promise.resolve();
        }
      }
    },
    pending: () => due.size,
  };
}

test('retries back off 5, 10, 20, 30 seconds, then every minute', () => {
  assert.deepEqual(RETRY_STEPS, [5, 10, 20, 30, 60]);
  assert.deepEqual(retrySchedule(7), [5000, 10000, 20000, 30000, 60000, 60000, 60000]);
  assert.equal(retryDelay(-3), 5000);
  assert.equal(retryDelay(Number.NaN), 5000);
  assert.equal(retryDelay(99), 60000);
});

test('the banner says offline, or counts down to the next try with Retry now', () => {
  assert.equal(bannerModel('online'), null);
  assert.deepEqual(bannerModel('offline'), {
    text: 'You’re offline — changes will be saved when you’re back',
    spoken: 'You’re offline. Changes will be saved when you’re back.',
    retry: false,
  });
  assert.equal(bannerModel('unreachable', 5).text, 'Can’t reach ShareCraft — retrying in 5 s');
  assert.equal(bannerModel('unreachable', 5).retry, true);
  assert.equal(bannerModel('unreachable', 0).text, 'Can’t reach ShareCraft — trying again…');
  assert.equal(bannerModel('trying').retry, false);
  // What screen readers hear does not change with the countdown.
  assert.equal(bannerModel('unreachable', 5).spoken, bannerModel('unreachable', 4).spoken);
});

test('a lost request starts the tries; each failure waits longer; an answer ends it', async () => {
  const clock = timers();
  const answers = [false, false, true];
  const changes = [];
  const link = createConnection({
    probe: async () => answers.shift(),
    ...clock,
    onChange: (state, info) => changes.push([state, info.back ? 'back' : info.due ?? null]),
  });
  link.lost();
  assert.equal(link.state, 'unreachable');
  assert.equal(link.due, 5000);
  link.lost(); // more failures meanwhile change nothing
  assert.equal(link.due, 5000);
  await clock.advance(5000);
  assert.equal(link.state, 'unreachable');
  assert.equal(link.attempt, 1);
  assert.equal(link.due, 15000, 'then 10 seconds');
  await clock.advance(10000);
  assert.equal(link.due, 35000, 'then 20 seconds');
  await clock.advance(20000);
  assert.equal(link.state, 'online');
  assert.deepEqual(changes, [
    ['unreachable', 5000],
    ['trying', null],
    ['unreachable', 15000],
    ['trying', null],
    ['unreachable', 35000],
    ['trying', null],
    ['online', 'back'],
  ]);
  assert.equal(clock.pending(), 0);
});

test('Retry now tries at once, and any answer brings the connection back', async () => {
  const clock = timers();
  let reachable = false;
  const link = createConnection({ probe: async () => reachable, ...clock });
  link.lost();
  await link.retry();
  assert.equal(link.attempt, 1);
  reachable = true;
  link.reached();
  assert.equal(link.state, 'online');
  assert.equal(link.attempt, 0);
});

test('offline waits for the browser; online tries at once; whenBack resolves or gives up', async () => {
  const clock = timers();
  const link = createConnection({ probe: async () => true, ...clock });
  assert.equal(await link.whenBack(), true, 'nothing wrong: at once');
  link.offline();
  assert.equal(link.state, 'offline');
  const waiting = link.whenBack(60_000);
  const late = link.whenBack(1_000);
  await clock.advance(1_000);
  assert.equal(await late, false, 'gave up');
  link.online();
  await Promise.resolve();
  await Promise.resolve();
  assert.equal(link.state, 'online');
  assert.equal(await waiting, true);
});
