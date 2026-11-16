import test from 'node:test';
import assert from 'node:assert/strict';
import {
  PAGE_SKELETONS,
  SHAPES,
  pageSkeleton,
  parseShapes,
  rememberShape,
  remembersCount,
  remembersHeight,
  remembersShown,
  shapeKey,
  skeletonCount,
} from './skeleton.js';

/** Every class used anywhere in a shape. */
function classesOf(node, into = new Set()) {
  if (node.classes) for (const name of node.classes) into.add(name);
  for (const child of node.children ?? []) classesOf(child, into);
  return into;
}
const bars = (node) => (node.bar ? 1 : (node.children ?? []).reduce((n, child) => n + bars(child), 0));

test('every page the brief names has a skeleton, with the site header band where the page has one', () => {
  assert.deepEqual(Object.keys(PAGE_SKELETONS).sort(), ['/access.html', '/bin.html', '/index.html', '/library.html', '/people.html', '/site.html']);
  assert.equal(pageSkeleton('/'), PAGE_SKELETONS['/index.html']);
  assert.equal(pageSkeleton('/doc.html'), null);
  for (const page of ['/site.html', '/library.html', '/access.html', '/bin.html']) {
    assert.equal(PAGE_SKELETONS[page].header, true, page);
    assert.equal(PAGE_SKELETONS[page].chrome, true, page);
  }
  // pages outside a site have the organisation's navigation bar, but no site header
  for (const page of ['/index.html', '/people.html']) {
    assert.equal(PAGE_SKELETONS[page].header, false, page);
    assert.equal(PAGE_SKELETONS[page].chrome, true, page);
  }
  const kinds = (page) => PAGE_SKELETONS[page].regions.map((region) => `${region.selector} ${region.kind}`);
  assert.deepEqual(kinds('/index.html'), ['#greeting title', '#recent recent-file', '#sites site-card']);
  assert.ok(kinds('/site.html').includes('#libraries library-card'));
  assert.ok(kinds('/library.html').includes('#file-rows file-row'));
  assert.ok(kinds('/access.html').includes('#assignments share-entry'));
  assert.ok(kinds('/people.html').includes('#users person'));
  assert.ok(kinds('/bin.html').includes('#groups bin-group'));
});

test('every region names a shape, and each shape has the real classes, so the sizes match', () => {
  for (const [page, spec] of Object.entries(PAGE_SKELETONS)) {
    for (const region of spec.regions) {
      if (['reserve', 'shown'].includes(region.kind)) continue;
      assert.ok(SHAPES[region.kind], `${page} ${region.kind}`);
    }
  }
  assert.deepEqual(SHAPES['site-card'].classes, ['site-card']);
  assert.ok(classesOf(SHAPES['site-card']).has('card-band'));
  assert.ok(classesOf(SHAPES['site-card']).has('card-body'));
  assert.deepEqual(SHAPES['library-card'].classes, ['library-card']);
  assert.ok(classesOf(SHAPES['library-card']).has('lib-pic'));
  assert.equal(SHAPES['file-row'].tag, 'tr');
  assert.deepEqual(
    SHAPES['file-row'].children.map((cell) => cell.classes[0]),
    ['c-select', 'c-name', 'c-modified', 'c-type', 'c-sharing', 'c-actions'],
    'the table keeps its columns',
  );
  assert.ok(classesOf(SHAPES['file-row']).has('sub'), 'the narrow layout keeps its second line');
  assert.ok(classesOf(SHAPES['share-entry']).has('avatar'), 'share rows have avatar circles');
  assert.ok(classesOf(SHAPES.person).has('avatar'));
  assert.deepEqual(SHAPES['bin-group'].classes, ['bin-group']);
  assert.equal(SHAPES['bin-group'].children[1].children.length, 3, 'three rows a group');
  // grey bars stand in for the text: a name, a date
  for (const kind of ['site-card', 'recent-file', 'library-card', 'file-row', 'share-entry', 'person', 'bin-group']) {
    assert.ok(bars(SHAPES[kind]) >= 2, kind);
  }
});

test('counts come from the last visit, else the default, never more than the most', () => {
  const region = { kind: 'file-row', count: 8, max: 14 };
  assert.equal(skeletonCount(region, undefined), 8);
  assert.equal(skeletonCount(region, 3), 3);
  assert.equal(skeletonCount(region, 0), 0, 'an empty folder shows no placeholder rows');
  assert.equal(skeletonCount(region, 50), 14);
  assert.equal(skeletonCount(region, -1), 8);
  assert.equal(skeletonCount(region, 2.5), 8);
  assert.equal(skeletonCount({ kind: 'title' }, 9), 1);
  assert.ok(remembersCount('site-card') && !remembersCount('title'));
  assert.ok(remembersHeight('reserve') && remembersHeight('about') && !remembersHeight('person'));
  assert.ok(remembersShown('shown') && !remembersShown('reserve'));
});

test('shapes are remembered per page and width, without the one-off welcome parameters', () => {
  assert.equal(shapeKey('/site.html', '?id=a&welcome=1&unset=look', '#libraries', true), '/site.html?id=a|#libraries|wide');
  assert.equal(shapeKey('/', '', 'header', false), '/index.html|header|narrow');
});

test('the memory keeps the newest keys, last used last', () => {
  let memory = {};
  for (let i = 0; i < 5; i += 1) memory = rememberShape(memory, `k${i}`, i, 3);
  assert.deepEqual(Object.keys(memory), ['k2', 'k3', 'k4']);
  memory = rememberShape(memory, 'k2', 9, 3);
  assert.deepEqual(Object.entries(memory), [['k3', 3], ['k4', 4], ['k2', 9]]);
});

test('stored shapes that cannot be read are forgotten', () => {
  assert.deepEqual(parseShapes('{"a":3,"b":"x","c":-1,"d":12.6,"e":99999}'), { a: 3, d: 13 });
  assert.deepEqual(parseShapes('nonsense'), {});
  assert.deepEqual(parseShapes('[1]'), {});
  assert.deepEqual(parseShapes(null), {});
});
