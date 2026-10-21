import assert from 'node:assert/strict';
import { test } from 'node:test';

import { lastChecked, updateStatus } from '../../src/components/about.js';

const RELEASES = 'https://github.com/YAPhoa/wayfetch-downloader/releases';
const URL = `${RELEASES}/tag/v0.2.0`;

/**
 * @param {import('../../src/client/contract.js').UpdateOutcome} outcome
 * @param {import('../../src/client/contract.js').UpdateInstall} [install]
 * @returns {import('../../src/client/contract.js').UpdateCheck}
 */
const check = (outcome, install) => ({ current: '0.1.0', releases: RELEASES, outcome, checked_at: 0, install });

const AVAILABLE = /** @type {const} */ ({ kind: 'available', version: '0.2.0', url: URL });
const PLAIN = { detail: '', link: null, restart: false, busy: false };

test('nothing is shown before the first check', () => {
  assert.equal(updateStatus(check({ kind: 'not_checked' })), null);
  assert.equal(lastChecked({ ...check({ kind: 'not_checked' }), checked_at: undefined }), 'Not checked yet');
  assert.match(lastChecked(check({ kind: 'up_to_date' })), /^Last checked /);
});

test('a newer release shows a badge and links to its page', () => {
  assert.deepEqual(updateStatus(check(AVAILABLE)), {
    ...PLAIN,
    badge: '0.2.0 available',
    icon: 'up',
    tone: 'run',
    link: { href: URL, label: 'Get 0.2.0' },
  });
});

test('no newer release shows a check mark and nothing more', () => {
  assert.deepEqual(updateStatus(check({ kind: 'up_to_date' })), {
    ...PLAIN,
    badge: 'Up to date',
    icon: 'check',
    tone: 'ok',
  });
});

test('a failed check says why and links every release', () => {
  assert.deepEqual(updateStatus(check({ kind: 'unavailable', problem: 'GitHub could not be reached' })), {
    ...PLAIN,
    badge: 'Failed',
    icon: 'cancel',
    tone: 'bad',
    detail: 'GitHub could not be reached.',
    link: { href: RELEASES, label: 'All releases' },
  });
});

test('the desktop app downloads a newer release, then offers to restart into it', () => {
  const downloading = updateStatus(check(AVAILABLE, { state: 'downloading', received: 420, total: 1000 }));
  assert.deepEqual(downloading, { ...PLAIN, badge: 'Downloading 42%', icon: 'down', tone: 'run', busy: true });
  assert.equal(updateStatus(check(AVAILABLE, { state: 'downloading', received: 420 }))?.badge, 'Downloading…');
  assert.deepEqual(updateStatus(check(AVAILABLE, { state: 'ready' })), {
    ...PLAIN,
    badge: '0.2.0 ready',
    icon: 'up',
    tone: 'ok',
    restart: true,
  });
  assert.deepEqual(updateStatus(check(AVAILABLE, { state: 'installing' })), {
    ...PLAIN,
    badge: 'Installing…',
    icon: 'more',
    tone: 'run',
    busy: true,
  });
  // A package that could not be got ready leaves the release's page.
  assert.deepEqual(updateStatus(check(AVAILABLE, { state: 'failed', problem: 'this release has no package for this app' })), {
    ...PLAIN,
    badge: 'Failed',
    icon: 'cancel',
    tone: 'bad',
    detail: 'This release has no package for this app.',
    link: { href: URL, label: 'Get 0.2.0' },
  });
});
