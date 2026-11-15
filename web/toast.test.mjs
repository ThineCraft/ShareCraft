import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { TOAST_DURATION, TOAST_LIMIT, createToastQueue } from './toast.js';
import { buildUndo, isUndoKey, performUndo, undoOutcome, undoResults } from './undo.js';

/** A clock the test moves by hand. */
function fakeClock() {
  let now = 0;
  let next = 1;
  const timers = new Map();
  return {
    now: () => now,
    set(fn, ms) {
      const id = next++;
      timers.set(id, { at: now + ms, fn });
      return id;
    },
    clear(id) {
      timers.delete(id);
    },
    advance(ms) {
      const until = now + ms;
      for (;;) {
        const due = [...timers.entries()].filter(([, t]) => t.at <= until).sort((a, b) => a[1].at - b[1].at)[0];
        if (!due) break;
        timers.delete(due[0]);
        now = due[1].at;
        due[1].fn();
      }
      now = until;
    },
    get pending() {
      return timers.size;
    },
  };
}

const A = '11111111-1111-4111-8111-111111111111';
const B = '22222222-2222-4222-8222-222222222222';
const C = '33333333-3333-4333-8333-333333333333';
const HERE = '44444444-4444-4444-8444-444444444444';
const PLANS = '55555555-5555-4555-8555-555555555555';
const ELSEWHERE = '66666666-6666-4666-8666-666666666666';

test('the queue adds, lists oldest first and dismisses', () => {
  const clock = fakeClock();
  const seen = [];
  const queue = createToastQueue({ clock, onChange: (list) => seen.push(list.map((t) => t.message)) });
  const one = queue.add({ message: 'One' });
  const two = queue.add({ message: 'Two', kind: 'error' });
  assert.deepEqual(queue.list().map((t) => [t.message, t.kind]), [['One', 'info'], ['Two', 'error']]);
  assert.equal(queue.dismiss(one), true);
  assert.equal(queue.dismiss(one), false);
  assert.deepEqual(queue.list().map((t) => t.id), [two]);
  assert.deepEqual(seen, [['One'], ['One', 'Two'], ['Two']]);
  assert.equal(clock.pending, 1);
});

test('at most three are shown; the oldest goes first and its timer stops', () => {
  const clock = fakeClock();
  const queue = createToastQueue({ clock });
  for (const word of ['a', 'b', 'c', 'd']) queue.add({ message: word });
  assert.equal(TOAST_LIMIT, 3);
  assert.deepEqual(queue.list().map((t) => t.message), ['b', 'c', 'd']);
  assert.equal(clock.pending, 3);
  const small = createToastQueue({ clock: fakeClock(), limit: 1 });
  small.add({ message: 'x' });
  small.add({ message: 'y' });
  assert.deepEqual(small.list().map((t) => t.message), ['y']);
});

test('each toast leaves after 8 seconds', () => {
  const clock = fakeClock();
  const queue = createToastQueue({ clock });
  queue.add({ message: 'First' });
  clock.advance(3000);
  queue.add({ message: 'Second' });
  assert.equal(TOAST_DURATION, 8000);
  clock.advance(4999);
  assert.equal(queue.list().length, 2);
  clock.advance(1);
  assert.deepEqual(queue.list().map((t) => t.message), ['Second']);
  clock.advance(3000);
  assert.deepEqual(queue.list(), []);
});

test('pausing keeps the time left; resuming restarts from there', () => {
  const clock = fakeClock();
  const queue = createToastQueue({ clock });
  const id = queue.add({ message: 'Held' });
  clock.advance(5000);
  queue.pause();
  assert.equal(queue.paused, true);
  assert.equal(queue.remaining(id), 3000);
  clock.advance(60_000);
  assert.equal(queue.list().length, 1);
  // A toast added while paused waits too.
  const later = queue.add({ message: 'Later' });
  clock.advance(60_000);
  assert.equal(queue.list().length, 2);
  queue.resume();
  queue.resume();
  clock.advance(2999);
  assert.equal(queue.list().length, 2);
  clock.advance(1);
  assert.deepEqual(queue.list().map((t) => t.id), [later]);
  clock.advance(5000);
  assert.deepEqual(queue.list(), []);
});

test('a toast with no end stays until dismissed; latestAction is the newest with an action', () => {
  const clock = fakeClock();
  const queue = createToastQueue({ clock });
  const undoOne = { label: 'Undo', run: () => {} };
  const undoTwo = { label: 'Undo', run: () => {} };
  queue.add({ message: 'Deleted a', action: undoOne });
  const second = queue.add({ message: 'Deleted b', action: undoTwo });
  queue.add({ message: 'Folder made' });
  const sticky = queue.add({ message: 'Stays', duration: Infinity });
  assert.equal(queue.latestAction().id, second);
  assert.equal(queue.latestAction().action, undoTwo);
  clock.advance(100_000);
  assert.deepEqual(queue.list().map((t) => t.id), [sticky]);
  assert.equal(queue.latestAction(), null);
});

test('buildUndo for a delete restores from the recycle bin', () => {
  const one = buildUndo({ op: 'delete', done: [A] }, [{ id: A, name: 'Budget.xlsx' }]);
  assert.equal(one.message, 'Deleted Budget.xlsx');
  assert.equal(one.label, 'Undo');
  assert.deepEqual(one.requests, [{ method: 'POST', path: `/bin/${A}/restore`, ids: [A], bulk: false }]);
  const some = buildUndo({ op: 'delete', done: [A, C] }, [
    { id: A, name: 'a' },
    { id: B, name: 'b' },
    { id: C, name: 'c' },
  ]);
  assert.equal(some.message, 'Deleted 2 items');
  assert.deepEqual(some.requests, [{ method: 'POST', path: '/bulk', body: { op: 'restore', items: [A, C] }, ids: [A, C], bulk: true }]);
  assert.equal(buildUndo({ op: 'delete', done: [] }, [{ id: A, name: 'a' }]), null);
  // More than one bulk request holds goes in batches.
  const many = Array.from({ length: 501 }, (_, i) => ({ id: `id-${i}`, name: `f${i}` }));
  const big = buildUndo({ op: 'delete', done: many.map((m) => m.id) }, many);
  assert.deepEqual(big.requests.map((r) => r.ids.length), [500, 1]);
});

test('buildUndo for a move moves each item back where it came from', () => {
  const one = buildUndo({ op: 'move', done: [A], to: { id: PLANS, name: 'Plans' } }, [{ id: A, name: 'Budget.xlsx', parent: HERE }]);
  assert.equal(one.message, 'Moved Budget.xlsx to Plans');
  assert.deepEqual(one.requests, [{ method: 'PATCH', path: `/nodes/${A}`, body: { parent: HERE }, ids: [A], bulk: false }]);
  const three = buildUndo({ op: 'move', done: [A, B, C], to: { id: PLANS, name: 'Plans' } }, [
    { id: A, name: 'a', parent: HERE },
    { id: B, name: 'b', parent: HERE },
    { id: C, name: 'c', parent: ELSEWHERE },
  ]);
  assert.equal(three.message, 'Moved 3 items to Plans');
  assert.deepEqual(three.requests, [
    { method: 'POST', path: '/bulk', body: { op: 'move', items: [A, B], parent: HERE }, ids: [A, B], bulk: true },
    { method: 'PATCH', path: `/nodes/${C}`, body: { parent: ELSEWHERE }, ids: [C], bulk: false },
  ]);
  assert.equal(buildUndo({ op: 'move', done: [A] }, [{ id: A, name: 'a' }]), null);
});

test('buildUndo for a rename renames back; nothing to undo when the name stayed', () => {
  const undo = buildUndo({ op: 'rename', id: A, name: 'Plan 2.docx' }, { id: A, name: 'Plan.docx' });
  assert.equal(undo.message, 'Renamed to Plan 2.docx');
  assert.deepEqual(undo.requests, [{ method: 'PATCH', path: `/nodes/${A}`, body: { name: 'Plan.docx' }, ids: [A], bulk: false }]);
  assert.equal(buildUndo({ op: 'rename', id: A, name: 'Same' }, { id: A, name: 'Same' }), null);
  assert.equal(buildUndo({ op: 'copy', done: [A] }, [{ id: A, name: 'a' }]), null);
  assert.equal(buildUndo(null, null), null);
});

test('undo results and outcomes in plain words', () => {
  const del = buildUndo({ op: 'delete', done: [A] }, [{ id: A, name: 'Budget.xlsx' }]);
  const single = undoResults(del.requests[0], { ok: true, data: { node: {}, renamed: null } });
  assert.deepEqual(undoOutcome(del, single), { ok: true, message: 'Restored Budget.xlsx.' });
  const renamed = undoResults(del.requests[0], { ok: true, data: { renamed: { id: A, from: 'Budget.xlsx', to: 'Budget (2).xlsx' } } });
  assert.equal(undoOutcome(del, renamed).message, 'Restored Budget.xlsx. “Budget.xlsx” is now “Budget (2).xlsx”, because its name was taken.');
  const refused = undoResults(del.requests[0], { ok: false, error: 'not_found' });
  assert.deepEqual(undoOutcome(del, refused), { ok: false, message: 'Could not undo. That could not be found.' });

  const move = buildUndo({ op: 'move', done: [A, B], to: { name: 'Plans' } }, [
    { id: A, name: 'a.txt', parent: HERE },
    { id: B, name: 'b.txt', parent: HERE },
  ]);
  const half = undoResults(move.requests[0], { ok: true, data: [{ id: A, ok: true }, { id: B, ok: false, error: 'name_taken' }] });
  const said = undoOutcome(move, half);
  assert.equal(said.ok, false);
  assert.match(said.message, /^Only 1 of 2 items were moved back\. b\.txt: Something here already has that name/);
  const whole = undoResults(move.requests[0], { ok: false, error: 'network' });
  assert.deepEqual(whole.map((r) => r.ok), [false, false]);
  assert.match(undoOutcome(move, whole).message, /^Could not undo\. None of the 2 items was moved back\./);
  assert.equal(undoOutcome(move, undoResults(move.requests[0], { ok: true, data: [{ id: A, ok: true }, { id: B, ok: true }] })).message, 'Moved 2 items back.');

  const rename = buildUndo({ op: 'rename', id: A, name: 'New.txt' }, { id: A, name: 'Old.txt' });
  assert.equal(undoOutcome(rename, [{ id: A, ok: true }]).message, 'Renamed back to Old.txt.');
});

test('performUndo sends each request and reports what happened', async () => {
  const calls = [];
  const move = buildUndo({ op: 'move', done: [A, C], to: { name: 'Plans' } }, [
    { id: A, name: 'a', parent: HERE },
    { id: C, name: 'c', parent: ELSEWHERE },
  ]);
  const outcome = await performUndo(move, async (method, path, body) => {
    calls.push([method, path, body]);
    return { ok: true, status: 200, data: { id: path.split('/').pop() } };
  });
  assert.deepEqual(calls, [
    ['PATCH', `/nodes/${A}`, { parent: HERE }],
    ['PATCH', `/nodes/${C}`, { parent: ELSEWHERE }],
  ]);
  assert.deepEqual(outcome, { ok: true, message: 'Moved 2 items back.' });
});

test('Ctrl+Z (or Cmd+Z) is Undo outside text fields only', () => {
  const button = { tagName: 'BUTTON' };
  assert.equal(isUndoKey({ key: 'z', ctrlKey: true, target: button }), true);
  assert.equal(isUndoKey({ key: 'Z', metaKey: true, target: button }), true);
  assert.equal(isUndoKey({ key: 'z', ctrlKey: true, shiftKey: true, target: button }), false);
  assert.equal(isUndoKey({ key: 'z', target: button }), false);
  assert.equal(isUndoKey({ key: 'z', ctrlKey: true, target: { tagName: 'INPUT', type: 'text' } }), false);
  assert.equal(isUndoKey({ key: 'z', ctrlKey: true, target: { tagName: 'INPUT', type: 'checkbox' } }), true);
  assert.equal(isUndoKey({ key: 'z', ctrlKey: true, target: { tagName: 'DIV', isContentEditable: true } }), false);
});

test('the library page deletes without a dialog and keeps the toast styles', () => {
  const html = readFileSync(new URL('./library.html', import.meta.url), 'utf8');
  assert.doesNotMatch(html, /id="delete-dialog"/);
  const css = readFileSync(new URL('./style.css', import.meta.url), 'utf8');
  assert.match(css, /\.toasts\{/);
  assert.match(css, /prefers-reduced-motion:no-preference\)\{\.toast\{animation/);
});
