import test from 'node:test';
import assert from 'node:assert/strict';

// link.js runs at import (top-level await), so each case loads a fresh copy against a minimal DOM
// stub and a scripted fetch. The token is a made-up value and is never printed.
const TOKEN = 'ab'.repeat(32);

function element() {
  return {
    hidden: false,
    textContent: '',
    href: '',
    listeners: {},
    attrs: {},
    children: [],
    addEventListener(type, fn) {
      this.listeners[type] = fn;
    },
    setAttribute(name, value) {
      this.attrs[name] = value;
    },
    append(...kids) {
      this.children.push(...kids);
    },
    replaceChildren(...kids) {
      this.children = kids;
    },
  };
}

function reply(status, body) {
  return { ok: status >= 200 && status < 300, status, json: async () => body };
}

let serial = 0;
async function load({ pathname = `/l/${TOKEN}`, stash, routes }) {
  const els = new Map();
  const stored = new Map(stash ? [['sharecraft-link', stash]] : []);
  const calls = [];
  const saved = {};
  const fakes = {
    document: {
      title: '',
      getElementById: (id) => {
        if (!els.has(id)) els.set(id, element());
        return els.get(id);
      },
      createElement: () => element(),
    },
    location: { pathname, origin: 'http://localhost', assign() {} },
    sessionStorage: {
      getItem: (key) => stored.get(key) ?? null,
      setItem: (key, value) => stored.set(key, value),
      removeItem: (key) => stored.delete(key),
    },
    fetch: async (url) => {
      const path = String(url).replace(`/api/l/${TOKEN}`, '');
      calls.push(path);
      return routes(path) ?? reply(404, { error: 'link_not_found' });
    },
  };
  for (const [name, value] of Object.entries(fakes)) {
    saved[name] = Object.getOwnPropertyDescriptor(globalThis, name);
    Object.defineProperty(globalThis, name, { value, configurable: true, writable: true });
  }
  const errors = [];
  try {
    await import(`./link.js?case=${serial++}`);
    // Let the folder or text load the page starts after showing the item finish.
    await new Promise((resolve) => setTimeout(resolve, 20));
  } catch (error) {
    errors.push(error);
  } finally {
    for (const [name, descriptor] of Object.entries(saved)) {
      if (descriptor) Object.defineProperty(globalThis, name, descriptor);
      else delete globalThis[name];
    }
  }
  return { els, stored, calls, errors };
}

const folder = { kind: 'folder', id: 'f1', name: 'Reports' };

test('an_anyone_folder_link_opens_and_lists_the_folder', async () => {
  const page = await load({
    routes: (path) => {
      if (path === '') return reply(200, { kind: 'anyone', item: folder });
      if (path.startsWith('/children/f1')) {
        return reply(200, {
          items: [{ kind: 'document', id: 'd1', name: 'Plan.txt', editor: 'text', revised_at: '2026-01-01T00:00:00Z' }],
          next: null,
        });
      }
      return null;
    },
  });
  assert.deepEqual(page.errors, []);
  assert.equal(page.els.get('kind').textContent, 'Folder');
  assert.equal(page.els.get('title').textContent, 'Reports');
  assert.ok(page.calls.some((path) => path.startsWith('/children/f1')));
  assert.equal(page.els.get('items').hidden, false);
  assert.equal(page.els.get('items').children.length, 1);
});

test('an_anyone_file_link_shows_the_file', async () => {
  const page = await load({
    routes: (path) =>
      path === ''
        ? reply(200, {
            kind: 'anyone',
            item: { kind: 'document', id: 'd1', name: 'Data.bin', editor: null, revised_at: '2026-01-01T00:00:00Z' },
          })
        : null,
  });
  assert.deepEqual(page.errors, []);
  assert.equal(page.els.get('kind').textContent, 'File');
  assert.equal(page.els.get('download').hidden, false);
  assert.ok(page.els.get('download').href.endsWith('/content/d1'));
});

test('an_anyone_text_document_link_shows_the_text', async () => {
  const page = await load({
    routes: (path) => {
      if (path === '') {
        return reply(200, {
          kind: 'anyone',
          item: { kind: 'document', id: 'd2', name: 'Note.txt', editor: 'text', revised_at: '2026-01-01T00:00:00Z' },
        });
      }
      if (path === '/text/d2') return reply(200, { name: 'Note.txt', revision: 3, text: 'hello' });
      return null;
    },
  });
  assert.deepEqual(page.errors, []);
  assert.equal(page.els.get('kind').textContent, 'Text document');
  assert.equal(page.els.get('text-body').textContent, 'hello');
  assert.equal(page.els.get('text-view').hidden, false);
});

test('a_signed_in_link_asks_to_confirm', async () => {
  const page = await load({
    routes: (path) => (path === '' ? reply(200, { kind: 'signed_in', role: { name: 'Editor' } }) : null),
  });
  assert.deepEqual(page.errors, []);
  assert.equal(page.els.get('open').hidden, false);
  assert.equal(page.els.get('open-button').textContent, 'Open with Editor access');
});

test('a_signed_out_visitor_is_sent_to_sign_in_and_the_token_is_kept', async () => {
  const page = await load({
    routes: (path) => (path === '' ? reply(401, { error: 'not_signed_in' }) : null),
  });
  assert.deepEqual(page.errors, []);
  assert.equal(page.els.get('signin').hidden, false);
  assert.equal(page.stored.get('sharecraft-link'), TOKEN);
});

test('the_token_kept_for_the_sign_in_trip_is_used_and_forgotten', async () => {
  const page = await load({
    pathname: '/link.html',
    stash: TOKEN,
    routes: (path) => (path === '' ? reply(200, { kind: 'anyone', item: folder }) : reply(200, { items: [], next: null })),
  });
  assert.deepEqual(page.errors, []);
  assert.equal(page.stored.has('sharecraft-link'), false);
  assert.equal(page.els.get('kind').textContent, 'Folder');
  assert.equal(page.els.get('empty').hidden, false);
});

test('a_link_that_does_not_work_shows_the_not_working_page', async () => {
  const page = await load({ routes: () => null });
  assert.deepEqual(page.errors, []);
  assert.equal(page.els.get('broken').hidden, false);
  const none = await load({ pathname: '/link.html', routes: () => null });
  assert.deepEqual(none.errors, []);
  assert.equal(none.els.get('broken').hidden, false);
});

test('an_anyone_link_previews_other_files_and_hides_held_ones', async () => {
  const other = { kind: 'document', id: 'd3', name: 'Photo.png', editor: 'other', revised_at: '2026-01-01T00:00:00Z' };
  const image = await load({ routes: (path) => (path === '' ? reply(200, { kind: 'anyone', item: other }) : null) });
  assert.deepEqual(image.errors, []);
  assert.equal(image.els.get('preview-view').hidden, false);
  const [img] = image.els.get('preview-body').children;
  assert.ok(img.attrs.src.endsWith('/preview/d3'));
  assert.equal(image.els.get('download').hidden, false);

  const page = { ...other, id: 'd4', name: 'page.html' };
  const markup = '<script>alert(1)</script>';
  const text = await load({
    routes: (path) => {
      if (path === '') return reply(200, { kind: 'anyone', item: page });
      if (path === '/preview/d4') return { ok: true, status: 200, text: async () => markup };
      return null;
    },
  });
  assert.deepEqual(text.errors, []);
  const [pre] = text.els.get('preview-body').children;
  assert.equal(pre.textContent, markup, 'a web page is shown as text');

  const listed = await load({
    routes: (path) => {
      if (path === '') return reply(200, { kind: 'anyone', item: folder });
      if (path.startsWith('/children/f1')) {
        return reply(200, { items: [{ ...other, scan: 'held' }, { ...other, id: 'd5', name: 'x.exe' }], next: null });
      }
      return null;
    },
  });
  assert.deepEqual(listed.errors, []);
  const [heldRow, plainRow] = listed.els.get('items').children;
  const words = (el) => (typeof el === 'string' ? [el] : [el.textContent, ...el.children.flatMap(words)]);
  assert.ok(words(heldRow).includes('Being checked for malware'));
  assert.ok(!words(heldRow).includes('Download'));
  assert.ok(words(plainRow).includes('Download'));
  assert.ok(!words(plainRow).includes('Preview'), 'no preview for a type the browser cannot show');
});
