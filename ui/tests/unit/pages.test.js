import assert from 'node:assert/strict';
import { test } from 'node:test';

import { pagePlace } from '../../src/client/pages.js';

test('a page knows its rows and its number out of the total', () => {
  assert.deepEqual(pagePlace({ earlier: 1, before: 50, size: 50, rows: 50, more: true, total: 120 }), {
    first: 51,
    last: 100,
    total: 120,
    page: 2,
    pages: 3,
  });
  assert.deepEqual(pagePlace({ earlier: 0, before: 0, size: 50, rows: 3, more: false, total: 3 }), {
    first: 1,
    last: 3,
    total: 3,
    page: 1,
    pages: 1,
  });
});

test('rows removed from earlier pages move the range, not the page number', () => {
  // 51 downloads, on page 2; one on page 1 was removed.
  assert.deepEqual(pagePlace({ earlier: 1, before: 49, size: 50, rows: 1, more: false, total: 50 }), {
    first: 50,
    last: 50,
    total: 50,
    page: 2,
    pages: 2,
  });
});

test('a lagging total never contradicts the page', () => {
  // Counts read before more downloads arrived.
  assert.deepEqual(pagePlace({ earlier: 0, before: 0, size: 50, rows: 50, more: true, total: 40 }), {
    first: 1,
    last: 50,
    total: 51,
    page: 1,
    pages: 2,
  });
});

test('without `before`, full earlier pages are counted', () => {
  assert.deepEqual(pagePlace({ earlier: 2, before: null, size: 100, rows: 7, more: false, total: null }), {
    first: 201,
    last: 207,
    total: null,
    page: 3,
    pages: null,
  });
});

test('an empty page has no range', () => {
  const place = pagePlace({ earlier: 1, before: 50, size: 50, rows: 0, more: false, total: 50 });
  assert.ok(place.first > place.last);
});
