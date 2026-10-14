// What a job allows and how to describe it: rules every view shares.

/**
 * @typedef {import('./contract.js').JobSummary} JobSummary
 * @typedef {import('./contract.js').JobAction} JobAction
 * @typedef {import('./contract.js').ProgressSample} ProgressSample
 */

/**
 * The commands a job accepts in its current state, mirroring the engine's
 * rules (`JobState::request_*`) so a view offers only those. The engine
 * still decides: a job may change before the command arrives.
 * @param {JobSummary} job
 * @returns {JobAction[]}
 */
export function availableActions(job) {
  /** @type {JobAction[]} */
  const actions = [];
  if (job.stage === 'publishing') return actions;
  if (job.desired === 'running' && job.stage !== 'complete' && job.stage !== 'failed') {
    actions.push('pause');
  }
  if (job.desired === 'paused') actions.push('resume');
  if (job.desired !== 'cancelled' && job.stage !== 'complete') actions.push('cancel');
  return actions;
}

/** Display names of states, by their protocol name. */
const STATE_LABELS = /** @type {Record<string, string>} */ ({
  needs_input: 'needs input',
});

/**
 * @param {string} state a `DisplayState` or `JobStage`
 */
export function stateLabel(state) {
  return STATE_LABELS[state] ?? state;
}

const UNITS = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];

/**
 * @param {number} bytes
 */
export function formatBytes(bytes) {
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < UNITS.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return unit === 0 ? `${bytes} B` : `${value.toFixed(1)} ${UNITS[unit]}`;
}

/**
 * A job's progress: durable bytes, or a live sample's when there is one.
 * @param {JobSummary} job
 * @param {ProgressSample | undefined} sample
 */
export function progress(job, sample) {
  const done = sample ? Math.max(sample.bytes_received, job.bytes_done) : job.bytes_done;
  const total = sample?.bytes_total ?? job.bytes_total ?? job.artifact?.size;
  const fraction = total ? Math.min(1, done / total) : undefined;
  const text = total ? `${formatBytes(done)} of ${formatBytes(total)}` : formatBytes(done);
  return {
    fraction,
    text,
    rate: sample ? `${formatBytes(sample.bytes_per_second)}/s` : undefined,
  };
}

/**
 * The artifact's file name, the last component of its path.
 * @param {JobSummary} job
 */
export function fileName(job) {
  const path = job.artifact?.path;
  return path === undefined ? undefined : path.slice(path.lastIndexOf('/') + 1);
}
