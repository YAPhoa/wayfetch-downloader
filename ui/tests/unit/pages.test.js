import assert from 'node:assert/strict';
import { test } from 'node:test';

import { pagePlace } from '../../src/client/pages.js';

test('a page knows its rows and its number out of the total', () => {
  assert.deepEqual(pagePlace({ earlier: 1, size: 50, rows: 50, more: true, total: 120 }), {
    first: 51,
    last: 100,
    total: 120,
    page: 2,
    pages: 3,
  });
  assert.deepEqual(pagePlace({ earlier: 0, size: 50, rows: 3, more: false, total: 3 }), {
    first: 1,
    last: 3,
    total: 3,
    page: 1,
    pages: 1,
  });
});

test('a lagging total never contradicts the page', () => {
  // Counts read before more downloads arrived.
  assert.deepEqual(pagePlace({ earlier: 0, size: 50, rows: 50, more: true, total: 40 }), {
    first: 1,
    last: 50,
    total: 51,
    page: 1,
    pages: 2,
  });
});

test('without a total, only the rows are known', () => {
  assert.deepEqual(pagePlace({ earlier: 2, size: 100, rows: 7, more: false, total: null }), {
    first: 201,
    last: 207,
    total: null,
    page: 3,
    pages: null,
  });
});
