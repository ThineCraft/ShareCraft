import test from 'node:test';
import assert from 'node:assert/strict';
import {
  PREFETCH_TTL,
  chromePaths,
  createPrefetchCache,
  followPaths,
  mapStore,
  mayKeep,
  mayPrefetchLink,
  prefetchKey,
  prefetchPlan,
  storageStore,
} from './prefetch.js';

const ORIGIN = 'https://share.example';
const SITE = '11111111-1111-4111-8111-111111111111';
const LIB = '22222222-2222-4222-8222-222222222222';
const FOLDER = '33333333-3333-4333-8333-333333333333';

/** A clock the test moves by hand. */
function clock(start = 1_000_000) {
  let t = start;
  return { now: () => t, move: (ms) => (t += ms) };
}

test('the key is the method and the path, for GET only', () => {
  assert.equal(prefetchKey('GET', `/nodes/${SITE}`), `GET /nodes/${SITE}`);
  assert.equal(prefetchKey('GET', '/sites?kind=team'), 'GET /sites?kind=team');
  for (const method of ['POST', 'PUT', 'PATCH', 'DELETE']) assert.equal(prefetchKey(method, '/sites'), null);
  assert.equal(prefetchKey('GET', 'sites'), null);
  assert.equal(prefetchKey('GET', undefined), null);
});

test('an answer is kept 15 seconds and used once', () => {
  const time = clock();
  const cache = createPrefetchCache({ store: mapStore(), now: time.now });
  assert.equal(PREFETCH_TTL, 15_000);
  const key = prefetchKey('GET', '/me');
  cache.put(key, { status: 200, data: { id: 'me' } });
  time.move(14_999);
  assert.equal(cache.has(key), true);
  assert.deepEqual(cache.take(key), { status: 200, data: { id: 'me' } });
  assert.equal(cache.take(key), null, 'used once');
  assert.equal(cache.has(key), false);
});

test('an answer older than 15 seconds is not used, and is dropped when asked for', () => {
  const time = clock();
  const map = new Map();
  const cache = createPrefetchCache({ store: mapStore(map), now: time.now });
  cache.put('GET /me', { status: 200, data: 1 });
  time.move(15_000);
  assert.equal(cache.has('GET /me'), false);
  assert.equal(cache.take('GET /me'), null);
  assert.equal(map.size, 0, 'a stale entry is removed');
});

test('sweep drops stale entries, clear drops all, and a broken store never throws', () => {
  const time = clock();
  const map = new Map();
  const cache = createPrefetchCache({ store: mapStore(map), now: time.now });
  cache.put('GET /a', { status: 200, data: 1 });
  time.move(10_000);
  cache.put('GET /b', { status: 200, data: 2 });
  time.move(6_000);
  cache.sweep();
  assert.deepEqual([...map.keys()], ['GET /b']);
  cache.clear();
  assert.equal(map.size, 0);
  map.set('GET /c', 'not json');
  assert.equal(cache.take('GET /c'), null);
  const broken = createPrefetchCache({
    store: { get: () => { throw new Error('no'); }, set: () => { throw new Error('full'); }, remove: () => { throw new Error('no'); }, keys: () => { throw new Error('no'); } },
  });
  assert.equal(broken.put('GET /x', { status: 200, data: 1 }), false);
  assert.equal(broken.take('GET /x'), null);
  assert.doesNotThrow(() => broken.sweep());
  assert.doesNotThrow(() => broken.clear());
});

test('the storage store keeps its own keys under the prefix and leaves others alone', () => {
  const data = new Map([['other', 'x']]);
  const storage = {
    get length() {
      return data.size;
    },
    key: (i) => [...data.keys()][i] ?? null,
    getItem: (k) => data.get(k) ?? null,
    setItem: (k, v) => data.set(k, String(v)),
    removeItem: (k) => data.delete(k),
  };
  const cache = createPrefetchCache({ store: storageStore(storage) });
  cache.put('GET /me', { status: 200, data: { id: 1 } });
  assert.ok(data.has('sharecraft-prefetch:GET /me'));
  cache.clear();
  assert.deepEqual([...data.keys()], ['other']);
});

test('only GET 200 JSON answers are kept; byte routes and no-store bytes never', () => {
  const json = { method: 'GET', status: 200, contentType: 'application/json', cacheControl: 'no-store' };
  assert.equal(mayKeep({ ...json, path: `/nodes/${SITE}` }), true, 'a JSON answer of the API');
  assert.equal(mayKeep({ ...json, path: `/nodes/${SITE}`, method: 'POST' }), false);
  assert.equal(mayKeep({ ...json, path: `/nodes/${SITE}`, status: 206 }), false);
  assert.equal(mayKeep({ ...json, path: `/nodes/${SITE}`, status: 404 }), false);
  for (const path of [`/docs/${SITE}/content`, `/docs/${SITE}/preview`, `/docs/${SITE}/thumbnail?size=s`, `/brand/images/${'a'.repeat(64)}`]) {
    assert.equal(mayKeep({ ...json, path }), false, path);
  }
  assert.equal(mayKeep({ method: 'GET', status: 200, path: `/x/${SITE}`, contentType: 'application/pdf', cacheControl: 'no-store' }), false, 'no-store bytes');
  assert.equal(mayKeep({ method: 'GET', status: 200, path: `/x/${SITE}`, contentType: 'text/plain', cacheControl: '' }), false, 'not JSON');
});

test('each page is planned the way its own code asks, so the keys match', () => {
  assert.deepEqual(prefetchPlan('/', ORIGIN), { paths: ['/me', '/settings/regional', '/navigation', '/hubs', '/sites'], follow: null });
  assert.deepEqual(prefetchPlan(`/?kind=team&hub=${SITE.toUpperCase()}&q=x`, ORIGIN).paths.at(-1), `/sites?kind=team&hub=${SITE}`);
  assert.deepEqual(prefetchPlan('/index.html?kind=nonsense', ORIGIN).paths.at(-1), '/sites');
  assert.deepEqual(prefetchPlan(`/site.html?id=${SITE}`, ORIGIN).paths, [
    '/me',
    `/nodes/${SITE}`,
    ...chromePaths(SITE),
    `/nodes/${SITE}/children?limit=500`,
  ]);
  assert.deepEqual(prefetchPlan(`/library.html?id=${FOLDER}`, ORIGIN), {
    paths: ['/me', `/nodes/${FOLDER}`, `/nodes/${FOLDER}/children?limit=50`],
    follow: { from: `/nodes/${FOLDER}`, library: true },
  });
  assert.deepEqual(prefetchPlan(`/access.html?id=${FOLDER}`, ORIGIN).paths, ['/me', `/nodes/${FOLDER}`, `/nodes/${FOLDER}/access`]);
  assert.deepEqual(prefetchPlan(`/bin.html?site=${SITE}`, ORIGIN).paths.at(-1), `/sites/${SITE}/bin?stage=first&limit=500`);
  assert.deepEqual(prefetchPlan(`/bin.html?site=${SITE}#second`, ORIGIN).paths.at(-1), `/sites/${SITE}/bin?stage=second&limit=500`);
  assert.deepEqual(prefetchPlan('/people.html', ORIGIN).paths, ['/me', '/settings/regional', '/navigation', '/admin/users']);
  assert.deepEqual(prefetchPlan('/groups.html', ORIGIN).paths, ['/me']);
});

test('nothing is planned for other hosts, unknown pages, downloads, sharing links or the page itself', () => {
  const none = { paths: [], follow: null };
  assert.deepEqual(prefetchPlan(`https://evil.example/site.html?id=${SITE}`, ORIGIN), none);
  assert.deepEqual(prefetchPlan('/signin.html', ORIGIN), none);
  assert.deepEqual(prefetchPlan(`/api/docs/${SITE}/content`, ORIGIN), none);
  assert.deepEqual(prefetchPlan('/l/abc', ORIGIN), none);
  assert.deepEqual(prefetchPlan(`/doc.html?id=${SITE}`, ORIGIN), none);
  assert.deepEqual(prefetchPlan('/site.html?id=not-an-id', ORIGIN), none);
  assert.deepEqual(prefetchPlan(`/site.html?id=${SITE}#libraries`, ORIGIN, `/site.html?id=${SITE}`), none);
  assert.deepEqual(prefetchPlan('http://[bad', ORIGIN), none);
});

test('a node answer leads to its library and its site chrome', () => {
  const node = { path: [{ kind: 'site', id: SITE }, { kind: 'library', id: LIB }, { kind: 'folder', id: FOLDER }] };
  assert.deepEqual(followPaths(node, { library: true }), [`/libraries/${LIB}`, ...chromePaths(SITE)]);
  assert.deepEqual(followPaths(node, { library: false }), chromePaths(SITE));
  assert.deepEqual(followPaths({ path: [{ kind: 'site', id: '../x' }] }), []);
  assert.deepEqual(followPaths(null), []);
});

test('only plain same-tab links are prefetched, and none with Save-Data on', () => {
  const link = (attributes = {}) => ({
    hasAttribute: (name) => name in attributes,
    getAttribute: (name) => attributes[name] ?? null,
  });
  assert.equal(mayPrefetchLink(link()), true);
  assert.equal(mayPrefetchLink(link({ target: '_self' })), true);
  assert.equal(mayPrefetchLink(link({ target: '_blank' })), false);
  assert.equal(mayPrefetchLink(link({ download: '' })), false);
  assert.equal(mayPrefetchLink(link({ 'data-no-prefetch': '' })), false);
  assert.equal(mayPrefetchLink(link(), { saveData: true }), false);
  assert.equal(mayPrefetchLink(null), false);
});
