import assert from 'node:assert/strict';
import { test } from 'node:test';

import { place } from '../../src/components/menu.js';

const viewport = { width: 800, height: 600 };
const size = { width: 200, height: 150 };

test('a menu opens below and right of its anchor when it fits', () => {
  assert.deepEqual(place({ left: 100, right: 120, top: 50, bottom: 70 }, size, viewport), {
    left: 100,
    top: 70,
  });
});

test('a menu near an edge flips to stay inside the window', () => {
  // Near the bottom right corner: above and to the left of the anchor.
  assert.deepEqual(place({ left: 700, right: 760, top: 500, bottom: 520 }, size, viewport), {
    left: 560,
    top: 350,
  });
});

test('a menu larger than the room is kept inside the window', () => {
  const at = place({ left: 10, right: 10, top: 10, bottom: 10 }, { width: 900, height: 700 }, viewport);
  assert.deepEqual(at, { left: 4, top: 4 });
  const pointer = place({ left: 799, right: 799, top: 599, bottom: 599 }, size, viewport);
  assert.ok(pointer.left >= 4 && pointer.left + size.width <= viewport.width - 4);
  assert.ok(pointer.top >= 4 && pointer.top + size.height <= viewport.height - 4);
});
