import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
  availableActions,
  canRemove,
  canRename,
  describe,
  displayLink,
  explain,
  fileKind,
  fileName,
  formatBytes,
  formatDuration,
  GROUPS,
  groupCount,
  jobName,
  jobSortValue,
  percent,
  progress,
  restartNote,
  retryPlan,
  unpackActions,
  unpackNote,
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

test('only published files are renamed, and only ended jobs removed', () => {
  assert.equal(canRename(job({ stage: 'complete', display: 'complete' })), true);
  for (const stage of /** @type {const} */ (['queued', 'transferring', 'publishing', 'failed', 'cancelled'])) {
    assert.equal(canRename(job({ stage })), false, stage);
  }
  for (const stage of /** @type {const} */ (['complete', 'failed', 'cancelled'])) {
    assert.equal(canRemove(job({ stage })), true, stage);
  }
  for (const stage of /** @type {const} */ (['queued', 'transferring', 'paused', 'publishing'])) {
    assert.equal(canRemove(job({ stage })), false, stage);
  }
});

test('actions mirror the engine’s rules', () => {
  assert.deepEqual(availableActions(job({ stage: 'transferring' })), ['pause', 'cancel']);
  assert.deepEqual(availableActions(job({ stage: 'paused', desired: 'paused' })), ['resume', 'cancel']);
  // Still pausing: it can be resumed or cancelled, not paused again.
  assert.deepEqual(availableActions(job({ stage: 'transferring', desired: 'paused' })), ['resume', 'cancel']);
  assert.deepEqual(availableActions(job({ stage: 'failed' })), ['cancel', 'retry']);
  assert.deepEqual(availableActions(job({ stage: 'publishing' })), []);
  assert.deepEqual(availableActions(job({ stage: 'complete' })), []);
  assert.deepEqual(availableActions(job({ stage: 'cancelled', desired: 'cancelled' })), ['retry']);
  // A cancel still stopping is not retried until it settles.
  assert.deepEqual(availableActions(job({ stage: 'transferring', desired: 'cancelled' })), []);
});

test('retry says whether it continues saved data and why a job started over', () => {
  assert.equal(retryPlan(job({ stage: 'failed', bytes_done: 2048 })), 'Continues from the 2.0 KiB saved when it can');
  assert.equal(retryPlan(job({ stage: 'cancelled' })), 'Starts the download again');
  assert.equal(restartNote(job({})), undefined);
  assert.equal(
    restartNote(job({ restart_reason: 'the file on the server changed since the saved data was downloaded' })),
    'Started over from the beginning: the file on the server changed since the saved data was downloaded.',
  );
});

test('downloads sort by what their rows show, amounts as numbers', () => {
  const mib = 1024 * 1024;
  const running = job({ name: 'b.zip', original_host: 'files.example', bytes_done: 4 * mib, bytes_total: 16 * mib });
  const sample = { boot_epoch: 1, job: /** @type {const} */ ('job-1'), generation: 1, bytes_received: 8 * mib, bytes_per_second: mib };
  assert.equal(jobSortValue(running, sample, 'name'), 'b.zip');
  assert.equal(jobSortValue(running, sample, 'host'), 'files.example');
  assert.equal(jobSortValue(running, sample, 'state'), describe(running).label);
  assert.equal(jobSortValue(running, sample, 'progress'), 0.5);
  assert.equal(jobSortValue(running, sample, 'size'), 16 * mib);
  assert.equal(jobSortValue(running, sample, 'speed'), mib);
  assert.equal(jobSortValue(running, sample, 'left'), 8);
  // A row that shows no amount sorts as having none.
  for (const column of /** @type {const} */ (['progress', 'size', 'speed', 'left'])) {
    assert.equal(jobSortValue(job({}), undefined, column), undefined, column);
  }
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
  assert.equal(progress(job({ bytes_done: 10, bytes_total: 0 }), undefined).percent, undefined);
  assert.equal(progress(job({ bytes_done: 0, bytes_total: 10 }), undefined).percent, '0%');
  assert.equal(progress(job({ bytes_done: 20, bytes_total: 10 }), undefined).fraction, 1);
  assert.equal(formatBytes(1023), '1023 B');
});

test('percentages keep one decimal and never round up to 100', () => {
  assert.deepEqual([0, 0.72, 0.57, 0.123, 0.9996, 1].map(percent), ['0%', '72%', '57%', '12.3%', '99.9%', '100%']);
});

test('every state filter but All has a status', () => {
  assert.deepEqual(
    GROUPS.map((g) => g.status),
    [null, 'active', 'paused', 'queued', 'failed', 'cancelled', 'done'],
  );
});

test('the file name is the artifact path’s last component', () => {
  assert.equal(fileName(job({})), undefined);
  assert.equal(fileName(job({ artifact: { root: 'root-1', path: 'a/b &lt;c&gt;.bin', size: 1 } })), 'b &lt;c&gt;.bin');
});

test('job names follow engine hints until publication or rename supplies the final file', () => {
  assert.equal(jobName(job({})), 'job-1');
  assert.equal(jobName(job({ name: 'Early Name.zip' })), 'Early Name.zip');
  const updated = job({ name: 'Provider Name.zip' });
  assert.equal(jobName(updated), 'Provider Name.zip');
  assert.equal(fileName(updated), undefined);
  assert.equal(canRename(updated), false);
  assert.equal(jobName(job({ ...updated, artifact: { root: 'root-1', path: 'renamed.zip', size: 1 } })), 'renamed.zip');
});

test('states read as plain words with what went wrong', () => {
  assert.deepEqual(describe(job({ stage: 'transferring', display: 'transferring' })), {
    label: 'Downloading',
    status: 'active',
  });
  assert.deepEqual(describe(job({ stage: 'transferring', display: 'pausing' })), { label: 'Pausing…', status: 'paused' });
  assert.equal(describe(job({ stage: 'transferring', display: 'stopping' })).status, 'cancelled');
  // A state this UI does not know keeps its name and stays neutral.
  const unknown = /** @type {import('../../src/client/contract.js').DisplayState} */ (/** @type {unknown} */ ('archiving'));
  assert.deepEqual(describe(job({ stage: 'transferring', display: unknown })), { label: 'archiving', status: null });
  const expired = { code: /** @type {const} */ ('lease_expired') };
  assert.deepEqual(describe(job({ stage: 'failed', display: 'failed', failure: expired })), {
    label: 'Failed: link expired',
    status: 'failed',
  });
  // A folder only the user can fix fails the download, which Retry continues.
  const folder = { code: /** @type {const} */ ('destination_unavailable') };
  const unavailable = job({ stage: 'failed', display: 'failed', failure: folder });
  assert.deepEqual(describe(unavailable), { label: 'Failed: folder unavailable', status: 'failed' });
  assert.ok(availableActions(unavailable).includes('retry'));
  assert.match(explain(expired), /Add the link again/);
});

test('account refusals retain the engine’s redacted authorization details and next step', () => {
  for (const message of [
    'Authorization failed at Real-Debrid (HTTP 401). Sign this account in again in Settings > Accounts & services.',
    'Forbidden by Real-Debrid (HTTP 403): access denied for this account. Check the account in Settings > Accounts & services.',
  ]) {
    assert.equal(explain({ code: 'invalid_credentials', message }), message);
  }
  for (const message of [undefined, null, '', '   ']) {
    assert.equal(
      explain({ code: 'invalid_credentials', message }),
      'The provider rejected the account. Check its credentials.',
    );
  }
});

test('each state filter counts the stages it shows', () => {
  /** @type {Parameters<typeof groupCount>[1]} */
  const counts = { queued: 4, transferring: 2, verifying: 1, failed: 4, complete: 9 };
  const count = Object.fromEntries(GROUPS.map((group) => [group.key, groupCount(group, counts)]));
  assert.deepEqual(count, {
    all: 20,
    active: 3,
    paused: 0,
    queued: 4,
    failed: 4,
    cancelled: 0,
    done: 9,
  });
});

test('file kinds come from the extension, including compound ones', () => {
  assert.equal(fileKind('report.zip'), 'ZIP archive');
  assert.equal(fileKind('Linux.Image.TAR.GZ'), 'Gzip-compressed tar archive');
  assert.equal(fileKind('notes.gz'), 'Gzip-compressed file');
  assert.equal(fileKind('movie.mkv'), 'Matroska video');
  assert.equal(fileKind('data.parquet'), 'PARQUET file');
  assert.equal(fileKind('download-1'), 'File with no extension');
  assert.equal(fileKind('.bashrc'), 'File with no extension');
  assert.equal(fileKind('odd.n%me'), 'File');
  // Names of Object.prototype members are not kinds.
  assert.equal(fileKind('a.constructor'), 'File');
  assert.equal(fileKind('a.__proto__'), 'File');
  assert.equal(fileKind('a.tar.__proto__'), 'File');
});

test('links are shown without credentials, query, or fragment', () => {
  assert.deepEqual(displayLink('https://user:secret@cdn.example.com:8443/a%20b/f.zip?sig=abc#part'), {
    text: 'https://cdn.example.com:8443/a%20b/f.zip',
    hidden: true,
  });
  assert.deepEqual(displayLink('https://example.com/f.zip'), { text: 'https://example.com/f.zip', hidden: false });
  assert.deepEqual(displayLink('https://example.com/f.zip?'), { text: 'https://example.com/f.zip', hidden: false });
  assert.equal(displayLink('not a link'), null);
});

test('a finished archive reads as its unpacking, and offers what the engine allows', () => {
  const archive = (
    /** @type {import('../../src/client/contract.js').ExtractionState} */ state,
    /** @type {string | undefined} */ problem = undefined,
  ) =>
    job({ stage: 'complete', display: 'complete', extraction: { state, problem } });
  assert.deepEqual(describe(archive('idle')), { label: 'Done', status: 'done' });
  assert.deepEqual(describe(archive('extracting')), { label: 'Unpacking…', status: 'active' });
  assert.deepEqual(describe(archive('needs_password')), { label: 'Needs password', status: 'paused' });
  assert.deepEqual(describe(archive('failed')), { label: 'Unpack failed', status: 'failed' });
  assert.deepEqual(unpackActions(archive('idle')), { start: { label: 'Unpack', password: false }, stop: false });
  assert.deepEqual(unpackActions(archive('complete')), {
    start: { label: 'Unpack again', password: false },
    stop: false,
  });
  assert.deepEqual(unpackActions(archive('needs_password')), {
    start: { label: 'Enter password…', password: true },
    stop: true,
  });
  for (const state of /** @type {const} */ (['waiting', 'queued', 'extracting'])) {
    assert.deepEqual(unpackActions(archive(state)), { start: null, stop: true }, state);
  }
  // Not an archive, or not finished: nothing to unpack.
  assert.deepEqual(unpackActions(job({ stage: 'complete', display: 'complete' })), { start: null, stop: false });
  assert.deepEqual(unpackActions({ ...archive('idle'), stage: 'queued' }), { start: null, stop: false });
  assert.equal(unpackNote(archive('waiting', 'Waiting for 2 more parts to download.')), 'Waiting for 2 more parts to download.');
  assert.equal(unpackNote(archive('complete')), undefined);
});
