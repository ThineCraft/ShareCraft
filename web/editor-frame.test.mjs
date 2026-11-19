import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { avatarColour } from './account.js';
import { backLabel, downloadNote, isFocusShortcut, liveLook, liveStatus, presenceModel, statusLabel } from './editor-frame.js';

const ME = '11111111-1111-1111-1111-111111111111';

test('presenceModel leaves this person out, every tab of theirs, and lists each other person once', () => {
  const model = presenceModel(
    [
      { client: 'c1', user: ME, name: 'Ann Tester', can_edit: true },
      { client: 'c2', user: 'u-bob', name: 'Bob Smith', can_edit: true },
      { client: 'c3', user: ME, name: 'Ann Tester', can_edit: true },
      { client: 'c4', user: 'u-bob', name: 'Bob Smith', can_edit: true },
      { client: 'c5', user: 'u-cat', name: 'Cat Jones', can_edit: false },
    ],
    ME,
  );
  assert.deepEqual(model.shown.map((person) => person.name), ['Bob Smith', 'Cat Jones']);
  assert.equal(model.more, 0);
  assert.equal(model.total, 2);
  assert.equal(model.shown[0].initials, 'BS');
  assert.equal(model.shown[0].colour, avatarColour('u-bob'));
  assert.equal(model.shown[1].readOnly, true);
  assert.equal(model.label, 'Also here: Bob Smith and Cat Jones (read only)');
});

test('presenceModel takes the text page\'s list (me flag, names only)', () => {
  const model = presenceModel(
    [
      { name: 'Ann Tester', me: true, can_edit: true },
      { name: 'Bob Smith', me: false, can_edit: true },
      { name: 'Bob Smith', me: false, can_edit: true },
      { name: '  ', me: false, can_edit: true },
    ],
    null,
  );
  assert.deepEqual(model.shown.map((person) => person.name), ['Bob Smith', 'Someone']);
  assert.equal(model.shown[0].colour, avatarColour('Bob Smith'));
});

test('presenceModel shows up to the limit, then counts the rest; every name stays in the label', () => {
  const people = ['A One', 'B Two', 'C Three', 'D Four', 'E Five', 'F Six'].map((name, at) => ({ user: `u${at}`, name }));
  const four = presenceModel(people, null);
  assert.equal(four.shown.length, 4);
  assert.equal(four.more, 2);
  assert.equal(four.label, 'Also here: A One, B Two, C Three, D Four, E Five and F Six');
  const exactly = presenceModel(people.slice(0, 4), null, 4);
  assert.equal(exactly.shown.length, 4);
  assert.equal(exactly.more, 0);
  const two = presenceModel(people, null, 2);
  assert.deepEqual([two.shown.length, two.more], [2, 4]);
});

test('presenceModel of nobody else is empty, whatever it is given', () => {
  for (const people of [[], null, undefined, 'x', [null, { name: 'Ann', me: true }]]) {
    const model = presenceModel(people, ME);
    assert.deepEqual([model.shown, model.more, model.total, model.label], [[], 0, 0, '']);
  }
});

test('statusLabel gives a quiet word per state', () => {
  assert.equal(statusLabel('saved').word, 'Saved');
  assert.equal(statusLabel('saving').word, 'Saving…');
  assert.equal(statusLabel('offline').word, 'Offline — changes kept');
  assert.equal(statusLabel('view').word, 'Read only');
  assert.equal(statusLabel('failed').word, 'Not saved');
  assert.equal(statusLabel('connecting').word, 'Connecting…');
  assert.equal(statusLabel('notice').word, 'Unsaved changes');
});

test('statusLabel keeps the longer sentence as the detail, and a given word wins', () => {
  assert.deepEqual(statusLabel({ tone: 'offline', text: 'Connection lost. Saving is paused.' }), {
    tone: 'offline',
    word: 'Offline — changes kept',
    detail: 'Connection lost. Saving is paused.',
  });
  assert.equal(statusLabel({ tone: 'saved', text: 'Saved' }).detail, '');
  assert.deepEqual(statusLabel({ tone: 'notice', text: 'That change is too large.', word: 'Saved' }), {
    tone: 'notice',
    word: 'Saved',
    detail: 'That change is too large.',
  });
  assert.equal(statusLabel({ tone: 'connecting', word: 'Being checked for viruses' }).word, 'Being checked for viruses');
  assert.equal(statusLabel({ tone: 'odd', text: 'Something' }).word, 'Something');
  assert.equal(statusLabel(null).word, '');
});

test('backLabel names the library and its site', () => {
  const site = { id: 's', name: 'Home' };
  assert.equal(backLabel({ id: 'l', name: 'Documents' }, site), 'Back to Documents in Home');
  assert.equal(backLabel(site, site), 'Back to Home');
  assert.equal(backLabel(null, site), 'Back to sites');
});

test('isFocusShortcut is Ctrl+Shift+F or Cmd+Shift+F only', () => {
  assert.equal(isFocusShortcut({ ctrlKey: true, shiftKey: true, key: 'F' }), true);
  assert.equal(isFocusShortcut({ metaKey: true, shiftKey: true, key: 'f' }), true);
  assert.equal(isFocusShortcut({ ctrlKey: true, shiftKey: true, key: 'Ƒ', code: 'KeyF' }), true);
  assert.equal(isFocusShortcut({ ctrlKey: true, key: 'f' }), false);
  assert.equal(isFocusShortcut({ shiftKey: true, key: 'F' }), false);
  assert.equal(isFocusShortcut({ ctrlKey: true, shiftKey: true, altKey: true, key: 'F' }), false);
  assert.equal(isFocusShortcut(null), false);
});

test('both editor pages have the slim bar, a status region, the strip and no site navigation row', () => {
  for (const page of ['file.html', 'doc.html']) {
    const html = readFileSync(new URL(`./${page}`, import.meta.url), 'utf8');
    for (const id of ['back', 'title', 'crumbs', 'status', 'presence', 'access-link', 'download', 'focus-toggle', 'more', 'more-menu', 'focus-exit', 'focus-status', 'banner', 'problem']) {
      assert.match(html, new RegExp(`id="${id}"`), `${page} has #${id}`);
    }
    assert.match(html, /class="status" role="status" aria-live="polite"/);
    assert.doesNotMatch(html, /style=|<style/);
    assert.doesNotMatch(html, /id="people"|id="docline"/);
  }
});

// ---- live documents (G5) ----

test('presence_hides_places_without_a_connection', () => {
  const model = presenceModel(
    [
      { participant: 1, user: 'u-bob', name: 'Bob Smith', can_edit: true, connected: true },
      { participant: 2, user: 'u-cat', name: 'Cat Jones', can_edit: true, connected: false },
      { participant: 3, user: 'u-dan', name: 'Dan Brown', can_edit: true },
    ],
    ME,
  );
  assert.deepEqual(model.shown.map((person) => person.name), ['Bob Smith', 'Dan Brown']);
  assert.equal(model.label, 'Also here: Bob Smith and Dan Brown');
  // A person with one dropped place and one connected tab is shown once.
  const both = presenceModel(
    [
      { participant: 1, user: 'u-bob', name: 'Bob Smith', connected: false },
      { participant: 4, user: 'u-bob', name: 'Bob Smith', connected: true },
    ],
    ME,
  );
  assert.equal(both.total, 1);
});

test('presence_shows_viewers_as_read_only', () => {
  const model = presenceModel([{ participant: 1, user: 'u-cat', name: 'Cat Jones', can_edit: false, connected: true }], ME);
  assert.equal(model.shown[0].readOnly, true);
  assert.equal(model.label, 'Also here: Cat Jones (read only)');
});

test('live_people_carry_colour_and_initials_for_the_editor', () => {
  const look = liveLook({ participant: 1, user: 'u-bob', name: 'Bob Smith' });
  assert.equal(look.initials, 'BS');
  assert.equal(look.colour, avatarColour('u-bob').light.bg);
  assert.match(look.colour, /^#[0-9a-f]{6}$/);
  // The bar's avatar colour for that account; the name stands in for a missing account.
  assert.equal(liveLook({ name: 'Cat Jones' }).colour, avatarColour('Cat Jones').light.bg);
  assert.equal(liveLook({}).initials, '?');
});

test('status_words_for_live_states', () => {
  const word = (state) => statusLabel(liveStatus(state)).word;
  const live = { phase: 'live', waiting: 0, canEdit: true };
  assert.equal(word(live), 'Saved');
  assert.equal(word({ ...live, waiting: 2 }), 'Saving…');
  assert.equal(word({ ...live, saving: true }), 'Saving…');
  assert.equal(word({ ...live, unsaved: true }), 'Unsaved changes kept');
  assert.equal(word({ ...live, paused: true, waiting: 1 }), 'Unsaved changes kept');
  assert.match(statusLabel(liveStatus({ ...live, paused: true })).detail, /paused/);
  assert.equal(word({ ...live, phase: 'offline', waiting: 1 }), 'Offline — changes kept');
  assert.equal(word({ ...live, phase: 'offline', unsaved: true }), 'Offline — changes kept');
  assert.equal(word({ ...live, phase: 'joining' }), 'Connecting…');
  assert.equal(word({ ...live, phase: 'catching_up' }), 'Connecting…');
  assert.equal(word({ ...live, canEdit: false }), 'Read only');
  assert.equal(word({ ...live, canEdit: false, phase: 'offline' }), 'Read only');
  assert.equal(word(null), 'Connecting…');
});

test('download_says_when_the_stored_file_is_behind', () => {
  const note = "The stored file doesn't include the latest changes yet";
  assert.equal(downloadNote({ live: { behind: true } }), note);
  assert.equal(downloadNote({ live: { behind: false } }), '');
  assert.equal(downloadNote({ live: { behind: false } }, { unsaved: true }), note);
  assert.equal(downloadNote({ live: { behind: true } }, { savedSince: true }), '');
  assert.equal(downloadNote({}), '');
  assert.equal(downloadNote(null), '');
});
