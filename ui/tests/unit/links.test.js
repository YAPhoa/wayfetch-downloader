import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
  addedWhen,
  count,
  entries,
  entriesProblem,
  linkName,
  linkStatus,
  matches,
  refusals,
  startable,
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

test('a started link stays listed as added to Downloads, and starts no more', () => {
  const started = link({ host: 'cdn.example', name_hint: 'film.mkv', job: 'job-7' });
  assert.deepEqual(linkStatus(started), { label: 'Added to Downloads', tone: 'ok', detail: null });
  assert.equal(startable(started), false);
  assert.equal(startable(link({ host: 'cdn.example' })), true);
  assert.equal(startable(link({ check: 'invalid', text: 'x' })), false);
});

test('the Added at column gives the time alone for today', () => {
  const now = new Date(2026, 9, 18, 16, 0);
  const time = (/** @type {Date} */ at) => at.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  const today = new Date(2026, 9, 18, 0, 43);
  assert.equal(addedWhen(today.getTime(), now), `Today, ${time(today)}`);
  const earlier = new Date(2026, 9, 17, 23, 5);
  const date = earlier.toLocaleDateString([], { day: 'numeric', month: 'short', year: 'numeric' });
  assert.equal(addedWhen(earlier.getTime(), now), `${date}, ${time(earlier)}`);
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
