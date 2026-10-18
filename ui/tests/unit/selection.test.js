import assert from 'node:assert/strict';
import { test } from 'node:test';

import { Selection } from '../../src/client/selection.js';

const rows = [{ id: 'a' }, { id: 'b' }, { id: 'c' }];

test('ticking and unticking rows, and what commands act on', () => {
  /** @type {Selection<string>} */
  const selection = new Selection();
  selection.pick(rows, true);
  selection.pick([rows[1]], false);
  assert.deepEqual([...selection], ['a', 'c']);
  // Only ticked rows that are shown, in the order shown.
  assert.deepEqual(selection.chosen([rows[2], rows[1], rows[0]]), [rows[2], rows[0]]);
  assert.deepEqual(selection.chosen([rows[1]]), []);
});

test('rows no longer listed are unticked', () => {
  /** @type {Selection<string>} */
  const selection = new Selection();
  selection.pick(rows, true);
  selection.keepOnly(['b', 'z']);
  assert.deepEqual([...selection], ['b']);
});

test('the select-all box: ticked, mixed, or off with nothing shown', () => {
  /** @type {Selection<string>} */
  const selection = new Selection();
  assert.deepEqual(selection.allState([]), { checked: false, indeterminate: false, disabled: true });
  assert.deepEqual(selection.allState(rows), { checked: false, indeterminate: false, disabled: false });
  selection.pick([rows[0]], true);
  assert.deepEqual(selection.allState(rows), { checked: false, indeterminate: true, disabled: false });
  // A hidden ticked row does not count toward the shown ones.
  assert.deepEqual(selection.allState([rows[0]]), { checked: true, indeterminate: false, disabled: false });
  selection.pick(rows, true);
  assert.deepEqual(selection.allState(rows), { checked: true, indeterminate: false, disabled: false });
});
