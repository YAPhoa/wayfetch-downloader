import assert from 'node:assert/strict';
import { test } from 'node:test';

import { cycleSort, saveSort, savedSort, sortedBy, toggleSort } from '../../src/client/sort.js';

test('a header click sorts by its column, and a second reverses it', () => {
  const byName = toggleSort({ column: 'added', descending: true }, 'name');
  assert.deepEqual(byName, { column: 'name', descending: false });
  assert.deepEqual(toggleSort(byName, 'name'), { column: 'name', descending: true });
  assert.deepEqual(toggleSort(toggleSort(byName, 'name'), 'name'), byName);
});

test('a third click on a cycling header goes back to the list order', () => {
  const unsorted = { column: null, descending: false };
  const ascending = cycleSort(unsorted, 'size');
  assert.deepEqual(ascending, { column: 'size', descending: false });
  assert.deepEqual(cycleSort(ascending, 'size'), { column: 'size', descending: true });
  assert.deepEqual(cycleSort(cycleSort(ascending, 'size'), 'size'), unsorted);
  assert.deepEqual(cycleSort({ column: 'name', descending: true }, 'size'), ascending);
});

test('text sorts as people read it, numbers as numbers, ties keep list order', () => {
  const items = [
    { id: 1, name: 'file10.bin', size: 30 },
    { id: 2, name: 'File2.bin', size: 4 },
    { id: 3, name: 'file2.bin', size: 200 },
  ];
  assert.deepEqual(
    sortedBy(items, (i) => i.name, false).map((i) => i.id),
    [2, 3, 1],
  );
  assert.deepEqual(
    sortedBy(items, (i) => i.name, true).map((i) => i.id),
    [1, 2, 3],
  );
  assert.deepEqual(
    sortedBy(items, (i) => i.size, false).map((i) => i.id),
    [2, 1, 3],
  );
  assert.deepEqual(items.map((i) => i.id), [1, 2, 3]);
});

test('rows without a value come last in either order', () => {
  const items = [{ id: 1, host: null }, { id: 2, host: 'b.example' }, { id: 3, host: undefined }, { id: 4, host: 'a.example' }];
  assert.deepEqual(
    sortedBy(items, (i) => i.host, false).map((i) => i.id),
    [4, 2, 1, 3],
  );
  assert.deepEqual(
    sortedBy(items, (i) => i.host, true).map((i) => i.id),
    [2, 4, 1, 3],
  );
});

test('a kept order is read back only when it names a known column', () => {
  /** @type {Map<string, string>} */
  const stored = new Map();
  const storage = {
    getItem: (/** @type {string} */ key) => stored.get(key) ?? null,
    setItem: (/** @type {string} */ key, /** @type {string} */ value) => void stored.set(key, value),
  };
  const fallback = { column: /** @type {'name' | 'added'} */ ('added'), descending: false };
  Object.defineProperty(globalThis, 'localStorage', { value: storage, configurable: true });
  try {
    assert.deepEqual(savedSort('k', ['name', 'added'], fallback), fallback);
    saveSort('k', { column: 'name', descending: true });
    assert.deepEqual(savedSort('k', ['name', 'added'], fallback), { column: 'name', descending: true });
    stored.set('k', '{"column":"gone","descending":true}');
    assert.deepEqual(savedSort('k', ['name', 'added'], fallback), fallback);
    stored.set('k', 'not json');
    assert.deepEqual(savedSort('k', ['name', 'added'], fallback), fallback);
  } finally {
    // @ts-ignore: removing the stand-in
    delete globalThis.localStorage;
  }
});
