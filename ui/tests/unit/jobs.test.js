import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
  availableActions,
  describe,
  explain,
  fileName,
  formatBytes,
  formatDuration,
  progress,
} from '../../src/client/jobs.js';

/**
 * @param {Partial<import('../../src/client/contract.js').JobSummary>} fields
 * @returns {import('../../src/client/contract.js').JobSummary}
 */
function job(fields) {
  return {
    id: 'job-1',
    package: 'pkg-1',
    stage: 'queued',
    desired: 'running',
    display: 'queued',
    revision: 1,
    generation: 1,
    bytes_done: 0,
    ...fields,
  };
}

test('actions mirror the engine’s rules', () => {
  assert.deepEqual(availableActions(job({ stage: 'transferring' })), ['pause', 'cancel']);
  assert.deepEqual(availableActions(job({ stage: 'paused', desired: 'paused' })), ['resume', 'cancel']);
  // Still pausing: it can be resumed or cancelled, not paused again.
  assert.deepEqual(availableActions(job({ stage: 'transferring', desired: 'paused' })), ['resume', 'cancel']);
  assert.deepEqual(availableActions(job({ stage: 'failed' })), ['cancel']);
  assert.deepEqual(availableActions(job({ stage: 'publishing' })), []);
  assert.deepEqual(availableActions(job({ stage: 'complete' })), []);
  assert.deepEqual(availableActions(job({ stage: 'cancelled', desired: 'cancelled' })), []);
});

test('progress uses durable bytes unless a sample is ahead', () => {
  const mib = 1024 * 1024;
  const base = job({ bytes_done: 8 * mib, bytes_total: 16 * mib });
  assert.deepEqual(progress(base, undefined), {
    fraction: 0.5,
    percent: '50%',
    text: '8.0 MiB of 16.0 MiB',
    size: '16.0 MiB',
    rate: undefined,
    left: undefined,
  });
  const sample = {
    boot_epoch: 1,
    job: /** @type {const} */ ('job-1'),
    generation: 1,
    bytes_received: 12 * mib,
    bytes_per_second: 2048,
  };
  assert.equal(progress(base, sample).text, '12.0 MiB of 16.0 MiB');
  assert.equal(progress(base, sample).rate, '2.0 KiB/s');
  assert.equal(progress(base, sample).left, '35 min');
  assert.equal(formatDuration(42.2), '43 s');
  assert.equal(formatDuration(3600), '1 h');
  assert.equal(formatDuration(3700), '1 h 2 min');
  assert.equal(progress(job({ bytes_done: 10 }), undefined).fraction, undefined);
  assert.equal(formatBytes(1023), '1023 B');
});

test('the file name is the artifact path’s last component', () => {
  assert.equal(fileName(job({})), undefined);
  assert.equal(fileName(job({ artifact: { root: 'root-1', path: 'a/b &lt;c&gt;.bin', size: 1 } })), 'b &lt;c&gt;.bin');
});

test('states read as plain words with what went wrong', () => {
  const clock = () => '17:42';
  assert.deepEqual(describe(job({ stage: 'transferring', display: 'transferring' })), {
    label: 'Downloading',
    tone: 'run',
  });
  assert.equal(describe(job({ stage: 'transferring', display: 'pausing' })).label, 'Pausing…');
  const expired = { code: /** @type {const} */ ('lease_expired') };
  assert.deepEqual(describe(job({ stage: 'failed', display: 'failed', failure: expired })), {
    label: 'Failed: link expired',
    tone: 'bad',
  });
  const later = { code: /** @type {const} */ ('request_timeout'), retry_at: 1_800_000_000_000 };
  assert.equal(
    describe(job({ stage: 'waiting', display: 'waiting', failure: later }), clock).label,
    'Waiting until 17:42',
  );
  assert.match(explain(expired), /Add the link again/);
});
