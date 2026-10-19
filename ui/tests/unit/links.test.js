import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
  addedWhen,
  checkable,
  count,
  linkName,
  linkStatus,
  matches,
  refusals,
  startable,
  textProblem,
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

const LIMITS = { max_page_size: 500, max_submit_items: 3, max_url_bytes: 20, max_collected_links: 10, max_collect_text_bytes: 2048 };

test('what stops text being collected stays within the published limits', () => {
  assert.equal(textProblem(' \n ', LIMITS), 'Paste or type at least one link.');
  // Bytes, not characters.
  assert.equal(textProblem('é'.repeat(1025), LIMITS), 'The text is longer than 2 KiB; collect it in parts.');
  // Finding the links, and marking entries that are not, is the engine's.
  assert.equal(textProblem('not a link', LIMITS), null);
});

test('an unchecked link names the file its path suggests', () => {
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
  assert.equal(checkable(invalid), false);
});

test('checks say what the server answered', () => {
  assert.deepEqual(linkStatus(link({ check: 'checking' })), { label: 'Checking', tone: 'calm', detail: null });
  assert.deepEqual(linkStatus(link({ check: 'online', size: 10 })), { label: 'Online', tone: 'ok', detail: null });
  assert.deepEqual(linkStatus(link({ check: 'offline', reason: 'not_found', check_detail: 'HTTP 404' })), {
    label: 'Offline',
    tone: 'bad',
    detail: 'The server says it does not exist. HTTP 404',
  });
  assert.deepEqual(linkStatus(link({ check: 'unknown', reason: 'blocked' })), {
    label: 'Could not check',
    tone: 'warn',
    detail: 'Wayfetch is not allowed to connect to this address.',
  });
  assert.ok(checkable(link({ check: 'offline' })));
});

test('a page read for files is started through them', () => {
  const read = link({ check: 'page', found: 2, skipped: 3 });
  assert.deepEqual(linkStatus(read), {
    label: 'Web page',
    tone: 'calm',
    detail: '2 files found on it, listed below it. 3 links to other pages left alone.',
  });
  assert.equal(startable(read), false);
  const unread = link({ check: 'page' });
  assert.equal(linkStatus(unread).detail, 'Not read. Analyze it to find the files on it.');
  // With nothing found on it, the engine decides what starting it does.
  assert.equal(startable(unread), true);
  assert.ok(matches(link({ found_on: 'https://a.example/release' }), 'release'));
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
