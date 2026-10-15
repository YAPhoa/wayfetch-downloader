import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
  batches,
  count,
  entries,
  entriesProblem,
  linkName,
  linkStatus,
  matches,
  refusals,
} from '../../src/client/links.js';

/**
 * @param {Partial<import('../../src/client/contract.js').CollectedLink>} fields
 * @returns {import('../../src/client/contract.js').CollectedLink}
 */
function link(fields) {
  return {
    id: 'src-1',
    batch: 'batch-1',
    collected_at: 0,
    revision: 1,
    check: 'not_checked',
    ...fields,
  };
}

const LIMITS = { max_page_size: 500, max_submit_items: 3, max_url_bytes: 20, max_collected_links: 10 };

test('entries are the words of pasted text, blank lines ignored', () => {
  assert.deepEqual(entries('  https://a.example/x\n\n\thttps://b.example/y  z\n'), [
    'https://a.example/x',
    'https://b.example/y',
    'z',
  ]);
  assert.deepEqual(entries(' \n '), []);
});

test('what stops entries being sent stays within the published limits', () => {
  assert.equal(entriesProblem([], LIMITS), 'Paste or type at least one link.');
  assert.equal(entriesProblem(['a', 'b', 'c', 'd'], LIMITS), 'At most 3 links at a time; this has 4.');
  // Bytes, not characters.
  assert.equal(entriesProblem(['https://a/', `https://${'é'.repeat(7)}`], LIMITS), 'Link 2 is longer than 20 bytes.');
  // An entry that is not a link is the engine's to mark, not a reason to send nothing.
  assert.equal(entriesProblem(['not a link'], LIMITS), null);
});

test('a valid link is only ever "Not checked"; its file name is a hint', () => {
  const valid = link({ host: 'cdn.example', name_hint: 'film.mkv' });
  assert.equal(linkName(valid), 'film.mkv');
  assert.deepEqual(linkStatus(valid), { label: 'Not checked', tone: 'calm', detail: null });
  assert.equal(linkName(link({ host: 'cdn.example' })), null);
  // A refused start is kept with its reason.
  assert.deepEqual(linkStatus(link({ problem: 'the folder is unavailable' })), {
    label: 'Not checked',
    tone: 'warn',
    detail: 'Not started: the folder is unavailable',
  });
  const invalid = link({ check: 'invalid', text: 'example.com/x', problem: 'not a valid URL' });
  assert.equal(linkName(invalid), 'example.com/x');
  assert.deepEqual(linkStatus(invalid), { label: 'Invalid link', tone: 'bad', detail: 'not a valid URL' });
});

test('links are grouped by batch in list order', () => {
  const grouped = batches([
    link({ id: 'src-1', batch: 'batch-1', collected_at: 5 }),
    link({ id: 'src-2', batch: 'batch-1', collected_at: 5 }),
    link({ id: 'src-3', batch: 'batch-2', collected_at: 9 }),
  ]);
  assert.deepEqual(
    grouped.map((g) => [g.batch, g.collectedAt, g.links.map((l) => l.id)]),
    [
      ['batch-1', 5, ['src-1', 'src-2']],
      ['batch-2', 9, ['src-3']],
    ],
  );
});

test('the filter matches name, host, or problem', () => {
  const l = link({ host: 'cdn.example', name_hint: 'Film.mkv', problem: 'the folder is unavailable' });
  assert.ok(matches(l, ''));
  assert.ok(matches(l, 'film'));
  assert.ok(matches(l, 'cdn'));
  assert.ok(matches(l, 'unavailable'));
  assert.ok(!matches(l, 'other'));
});

test('refusals read as one sentence per reason', () => {
  assert.equal(count(1, 'link'), '1 link');
  assert.equal(count(2, 'entry is', 'entries are'), '2 entries are');
  const refused = (/** @type {string} */ id, /** @type {string} */ message) => ({
    link: id,
    code: /** @type {const} */ ('revision_conflict'),
    message,
  });
  assert.equal(
    refusals([refused('src-1', 'it changed.'), refused('src-2', 'it changed.'), refused('src-3', 'it is gone.')]),
    '2 links: it changed. it is gone.',
  );
});
