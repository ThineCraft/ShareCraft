// The phone-first shell's pure parts (round 20): when it is on, the keyboard, long press and sheets.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  SHEET_CLOSED,
  createLongPress,
  keyboardUp,
  phoneShellNow,
  sheetReducer,
  shellMode,
} from './sheet.js';

test('shellMode: phones below 700 px, fingers below 900 px', () => {
  assert.equal(shellMode(375, 'coarse'), 'phone');
  assert.equal(shellMode(375, 'fine'), 'phone');
  assert.equal(shellMode(699, 'fine'), 'phone');
  assert.equal(shellMode(700, 'fine'), 'desktop');
  assert.equal(shellMode(768, 'coarse'), 'phone');
  assert.equal(shellMode(899, 'coarse'), 'phone');
  assert.equal(shellMode(900, 'coarse'), 'desktop');
  assert.equal(shellMode(1280, 'coarse'), 'desktop');
  assert.equal(shellMode(800, 'none'), 'desktop');
  assert.equal(shellMode(undefined), 'desktop');
  assert.equal(shellMode(0, 'coarse'), 'desktop');
  assert.equal(shellMode('375'), 'phone');
});

test('phoneShellNow reads the window', () => {
  const win = (width, coarse) => ({ innerWidth: width, matchMedia: (query) => ({ matches: coarse && query.includes('coarse') }) });
  assert.equal(phoneShellNow(win(375, false)), true);
  assert.equal(phoneShellNow(win(820, true)), true);
  assert.equal(phoneShellNow(win(820, false)), false);
  assert.equal(phoneShellNow({}), false);
});

test('keyboardUp tells the keyboard from the browser bars', () => {
  assert.equal(keyboardUp(812, 812), false);
  assert.equal(keyboardUp(812, 750), false); // the address bar coming back
  assert.equal(keyboardUp(812, 480), true);
  assert.equal(keyboardUp(400, 280), true); // a short landscape screen: a quarter is enough
  assert.equal(keyboardUp(undefined, 300), false);
});

test('long press: held still for the delay presses; the lift is swallowed', () => {
  const press = createLongPress({ delay: 500, slop: 10 });
  assert.equal(press.down({ x: 100, y: 200, pointerId: 1, button: 0 }, 1000), 'waiting');
  assert.equal(press.tick(1300), null);
  assert.equal(press.move({ x: 104, y: 203, pointerId: 1 }, 1350), 'waiting'); // a tremble
  assert.equal(press.tick(1500), 'pressed');
  assert.equal(press.state(), 'pressed');
  assert.equal(press.move({ x: 140, y: 260, pointerId: 1 }, 1600), null); // moving after does not undo it
  assert.equal(press.up({ x: 140, y: 260, pointerId: 1 }, 1700), 'after-press');
  assert.equal(press.state(), 'idle');
});

test('long press: a quick tap is a tap', () => {
  const press = createLongPress();
  press.down({ x: 0, y: 0 }, 0);
  assert.equal(press.up({ x: 1, y: 1 }, 120), 'tap');
  assert.equal(press.tick(600), null);
});

test('long press: a scroll (moving past the slop) never presses', () => {
  const press = createLongPress({ delay: 500, slop: 10 });
  press.down({ x: 50, y: 300, pointerId: 7 }, 0);
  assert.equal(press.move({ x: 50, y: 289, pointerId: 7 }, 80), 'cancelled');
  assert.equal(press.tick(600), null);
  assert.equal(press.up({ x: 50, y: 100, pointerId: 7 }, 700), null);
  // the browser taking the pointer for its own scroll cancels too
  press.down({ x: 50, y: 300 }, 1000);
  assert.equal(press.cancel(), 'cancelled');
  assert.equal(press.tick(2000), null);
});

test('long press: a late timer still presses on lift or move; other pointers and buttons are ignored', () => {
  const press = createLongPress({ delay: 500 });
  press.down({ x: 0, y: 0, pointerId: 1 }, 0);
  assert.equal(press.down({ x: 9, y: 9, pointerId: 2 }, 10), null); // a second finger
  assert.equal(press.move({ x: 50, y: 50, pointerId: 2 }, 20), null);
  assert.equal(press.up({ x: 0, y: 0, pointerId: 1 }, 650), 'pressed');
  press.down({ x: 0, y: 0 }, 1000);
  assert.equal(press.move({ x: 2, y: 2 }, 1510), 'pressed');
  press.up({}, 1600);
  assert.equal(press.down({ x: 0, y: 0, button: 2 }, 2000), null); // a right button
  assert.equal(press.state(), 'idle');
});

test('sheet: opens, drags with the finger, springs back from a short pull', () => {
  let state = sheetReducer(undefined, { type: 'open' });
  assert.equal(state.phase, 'open');
  state = sheetReducer(state, { type: 'drag-start', y: 500, at: 0 });
  assert.equal(state.phase, 'dragging');
  state = sheetReducer(state, { type: 'drag-move', y: 540, at: 100 });
  assert.equal(state.offset, 40);
  state = sheetReducer(state, { type: 'drag-move', y: 480, at: 200 });
  assert.equal(state.offset, 0, 'never pulled above where it rests');
  state = sheetReducer(state, { type: 'drag-move', y: 560, at: 400 });
  state = sheetReducer(state, { type: 'drag-end', y: 560, at: 600 });
  assert.equal(state.phase, 'open');
  assert.equal(state.offset, 0);
});

test('sheet: a long pull or a flick down closes; close and open from anywhere', () => {
  let state = sheetReducer(sheetReducer(undefined, { type: 'open' }), { type: 'drag-start', y: 400, at: 0 });
  state = sheetReducer(state, { type: 'drag-move', y: 520, at: 300 });
  state = sheetReducer(state, { type: 'drag-end', y: 520, at: 400 });
  assert.deepEqual(state, SHEET_CLOSED);

  state = sheetReducer(sheetReducer(undefined, { type: 'open' }), { type: 'drag-start', y: 400, at: 0 });
  state = sheetReducer(state, { type: 'drag-move', y: 410, at: 20 });
  state = sheetReducer(state, { type: 'drag-end', y: 450, at: 50 }); // 40 px in 30 ms
  assert.equal(state.phase, 'closed');

  assert.equal(sheetReducer({ phase: 'open', offset: 0 }, { type: 'close' }).phase, 'closed');
  assert.equal(sheetReducer(SHEET_CLOSED, { type: 'drag-start', y: 0, at: 0 }).phase, 'closed', 'a closed sheet is not dragged');
  assert.equal(sheetReducer(SHEET_CLOSED, { type: 'nonsense' }), SHEET_CLOSED);
});
