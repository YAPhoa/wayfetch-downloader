import assert from 'node:assert/strict';
import { test } from 'node:test';

import { updateMessage } from '../../src/components/about.js';

const RELEASES = 'https://github.com/YAPhoa/wayfetch-downloader/releases';

/**
 * @param {import('../../src/client/contract.js').UpdateOutcome} outcome
 */
const check = (outcome) => ({ current: '0.1.0', releases: RELEASES, outcome });

test('a newer release links to its page', () => {
  const url = `${RELEASES}/tag/v0.2.0`;
  assert.deepEqual(updateMessage(check({ kind: 'available', version: '0.2.0', url })), {
    text: 'Version 0.2.0 is available.',
    tone: '',
    link: { href: url, label: 'View release' },
  });
});

test('no newer release needs nothing more', () => {
  assert.deepEqual(updateMessage(check({ kind: 'up_to_date' })), {
    text: 'Wayfetch is up to date.',
    tone: '',
    link: null,
  });
});

test('a failed check says why and links every release', () => {
  assert.deepEqual(updateMessage(check({ kind: 'unavailable', problem: 'GitHub could not be reached' })), {
    text: 'Could not check for updates: GitHub could not be reached.',
    tone: 'error',
    link: { href: RELEASES, label: 'All releases' },
  });
});
