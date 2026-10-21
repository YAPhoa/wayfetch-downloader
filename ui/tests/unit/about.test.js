import assert from 'node:assert/strict';
import { test } from 'node:test';

import { updateStatus } from '../../src/components/about.js';

const RELEASES = 'https://github.com/YAPhoa/wayfetch-downloader/releases';

/**
 * @param {import('../../src/client/contract.js').UpdateOutcome} outcome
 */
const check = (outcome) => ({ current: '0.1.0', releases: RELEASES, outcome });

test('a newer release shows a badge and links to its page', () => {
  const url = `${RELEASES}/tag/v0.2.0`;
  assert.deepEqual(updateStatus(check({ kind: 'available', version: '0.2.0', url })), {
    badge: '0.2.0 available',
    icon: 'up',
    tone: 'run',
    detail: '',
    link: { href: url, label: 'Get 0.2.0' },
  });
});

test('no newer release shows a check mark and nothing more', () => {
  assert.deepEqual(updateStatus(check({ kind: 'up_to_date' })), {
    badge: 'Up to date',
    icon: 'check',
    tone: 'ok',
    detail: '',
    link: null,
  });
});

test('a failed check says why and links every release', () => {
  assert.deepEqual(updateStatus(check({ kind: 'unavailable', problem: 'GitHub could not be reached' })), {
    badge: 'Failed',
    icon: 'cancel',
    tone: 'bad',
    detail: 'GitHub could not be reached.',
    link: { href: RELEASES, label: 'All releases' },
  });
});
