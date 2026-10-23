import assert from 'node:assert/strict';
import { test } from 'node:test';

import { describeRule, parseSize, sizeFields, splitExtensions, waitsForCheck } from '../../src/client/filters.js';
import { collectedText } from '../../src/components/collect.js';

test('a filter is summed up in one line, condition by condition', () => {
  assert.equal(
    describeRule({ name: 'Ads', enabled: true, link: { op: 'contains', pattern: 'ads.example' } }),
    'Link contains “ads.example”',
  );
  assert.equal(
    describeRule({
      name: 'Small samples',
      enabled: true,
      file_name: { op: 'is', pattern: '.*sample.*', regex: true },
      size: { op: 'is', min: 0, max: 1048576 },
      file_type: { op: 'is_not', kinds: ['video'], extensions: ['nfo'] },
      check: { op: 'is', values: ['offline', 'unknown'] },
    }),
    'File name is /.*sample.*/ and size at most 1.0 MiB and type is not video or .nfo and check is offline or could not tell',
  );
  assert.equal(
    describeRule({ name: 'Big', enabled: true, size: { op: 'is_not', min: 1024 } }),
    'Size not at least 1.0 KiB',
  );
  assert.equal(
    describeRule({
      name: 'Pages',
      enabled: true,
      added_from: { op: 'is', values: ['analyzed_page'] },
      check: { op: 'is', values: ['online', 'unknown'] },
      route: { op: 'is_not', values: ['account'] },
    }),
    'Added from an analyzed page and check is online or could not tell and not downloaded through an account',
  );
});

test('only conditions a check tells make a filter wait for it', () => {
  assert.equal(waitsForCheck({ name: 'a', enabled: true, link: { op: 'is', pattern: 'x' } }), false);
  assert.equal(waitsForCheck({ name: 'a', enabled: true, route: { op: 'is', values: ['account'] } }), true);
});

test('sizes are typed with a unit and shown in the largest whole one', () => {
  assert.equal(parseSize('', 'MiB'), null);
  assert.equal(parseSize('1.5', 'KiB'), 1536);
  assert.ok(Number.isNaN(parseSize('lots', 'B')));
  assert.deepEqual(sizeFields(3 * 1024 ** 3), { amount: '3', unit: 'GiB' });
  assert.deepEqual(sizeFields(1536), { amount: '1536', unit: 'B' });
});

test('extensions are typed as a list, with or without dots', () => {
  assert.deepEqual(splitExtensions(' .nfo, r* ,, sfv '), ['nfo', 'r*', 'sfv']);
});

test('a collect says how many links its filters left out', () => {
  assert.equal(
    collectedText({ collected: 2, invalid: 0, filtered: 3 }),
    'Collected 2 links for review. 3 links left out by your link filters.',
  );
});
