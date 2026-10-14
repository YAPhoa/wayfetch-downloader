// What a job allows and how to describe it: rules every view shares.

/**
 * @typedef {import('./contract.js').JobSummary} JobSummary
 * @typedef {import('./contract.js').JobAction} JobAction
 * @typedef {import('./contract.js').JobStage} JobStage
 * @typedef {import('./contract.js').DisplayState} DisplayState
 * @typedef {import('./contract.js').ProgressSample} ProgressSample
 * @typedef {import('./generated/FailureCode.js').FailureCode} FailureCode
 * @typedef {import('./generated/FailureSummary.js').FailureSummary} FailureSummary
 *
 * How a state reads at a glance: running, waiting on something, quietly
 * stopped, gone wrong, or finished.
 * @typedef {'run' | 'warn' | 'calm' | 'bad' | 'ok'} Tone
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

/**
 * Each state's plain name and tone.
 * @type {Readonly<Record<DisplayState, [string, Tone]>>}
 */
const STATES = Object.freeze({
  queued: ['Queued', 'calm'],
  resolving: ['Starting', 'run'],
  transferring: ['Downloading', 'run'],
  verifying: ['Checking file', 'run'],
  publishing: ['Saving', 'run'],
  recovering: ['Recovering', 'run'],
  waiting: ['Waiting', 'warn'],
  needs_input: ['Needs your answer', 'warn'],
  paused: ['Paused', 'calm'],
  blocked: ['Blocked', 'warn'],
  complete: ['Done', 'ok'],
  failed: ['Failed', 'bad'],
  cancelled: ['Cancelled', 'calm'],
  pausing: ['Pausing…', 'calm'],
  stopping: ['Stopping…', 'calm'],
});

/**
 * What each failure means, briefly for a list and as what happened plus
 * what to do for a job's details.
 * @type {Readonly<Record<FailureCode, { short: string, explain: string }>>}
 */
const FAILURES = Object.freeze({
  request_timeout: {
    short: 'timed out',
    explain: 'The source stopped answering. Wayfetch tries again on its own.',
  },
  lease_expired: {
    short: 'link expired',
    explain:
      'The download link expired before the file finished. Add the link again to get a fresh one.',
  },
  lease_not_found: {
    short: 'link not found',
    explain: 'The download link stopped working. Add the link again to get a fresh one.',
  },
  operation_ambiguous: {
    short: 'outcome unknown',
    explain:
      'Wayfetch could not tell whether its last request went through, so it did not repeat it. Check the source before trying again.',
  },
  token_expired: {
    short: 'sign-in expired',
    explain: 'The account’s sign-in expired. Wayfetch renews it on its own.',
  },
  invalid_credentials: {
    short: 'account rejected',
    explain: 'The provider rejected the account. Check its credentials.',
  },
  quota_exhausted: {
    short: 'quota used up',
    explain: 'The account’s quota is used up. Wayfetch waits until it resets.',
  },
  host_unavailable_via_provider: {
    short: 'host unavailable',
    explain: 'The provider cannot serve this host right now.',
  },
  resource_deleted: {
    short: 'file deleted',
    explain: 'The source says the file was deleted.',
  },
  challenge_required: {
    short: 'needs an answer',
    explain: 'The source asks for a password, CAPTCHA or another answer.',
  },
  adapter_parse_failure: {
    short: 'unexpected answer',
    explain: 'The source answered in a way Wayfetch does not understand.',
  },
  destination_unavailable: {
    short: 'folder unavailable',
    explain: 'The download folder is full, missing or unavailable. Fix the folder to continue.',
  },
  permission_denied: {
    short: 'access denied',
    explain: 'Wayfetch may not write to the download folder. Check its permissions.',
  },
  integrity_mismatch: {
    short: 'file check failed',
    explain: 'The received bytes did not match what was expected.',
  },
  missing_archive_volume: {
    short: 'archive part missing',
    explain: 'A part of the archive set is missing.',
  },
  unsupported_capability: {
    short: 'not supported',
    explain: 'Wayfetch cannot handle this link yet.',
  },
  user_cancelled: {
    short: 'cancelled',
    explain: 'You cancelled this download.',
  },
});

/**
 * @param {...JobStage} list
 * @returns {readonly JobStage[]}
 */
function stages(...list) {
  return Object.freeze(list);
}

/**
 * The state filters a list offers, with the stages each one shows.
 * @type {readonly { key: string, label: string, tone: Tone, stages: readonly JobStage[] | null }[]}
 */
export const GROUPS = Object.freeze([
  { key: 'all', label: 'All', tone: 'calm', stages: null },
  {
    key: 'active',
    label: 'Active',
    tone: 'run',
    stages: stages('resolving', 'transferring', 'verifying', 'publishing', 'recovering'),
  },
  { key: 'waiting', label: 'Waiting', tone: 'warn', stages: stages('waiting', 'needs_input', 'blocked') },
  { key: 'paused', label: 'Paused', tone: 'calm', stages: stages('paused') },
  { key: 'queued', label: 'Queued', tone: 'calm', stages: stages('queued') },
  { key: 'failed', label: 'Failed', tone: 'bad', stages: stages('failed') },
  { key: 'cancelled', label: 'Cancelled', tone: 'calm', stages: stages('cancelled') },
  { key: 'done', label: 'Done', tone: 'ok', stages: stages('complete') },
]);

/**
 * A job's state in plain words, such as "Waiting until 17:42" or
 * "Failed: link expired", and its tone.
 * @param {JobSummary} job
 * @param {(time: Date) => string} [clock] formats a retry time
 * @returns {{ label: string, tone: Tone }}
 */
export function describe(job, clock = shortTime) {
  const [name, tone] = STATES[job.display];
  const failure = job.failure;
  if (job.display === 'waiting' && failure?.retry_at !== undefined && failure.retry_at !== null) {
    return { label: `Waiting until ${clock(new Date(failure.retry_at))}`, tone };
  }
  const explained = job.display === 'failed' || job.display === 'blocked' || job.display === 'waiting';
  if (failure && explained) {
    return { label: `${name}: ${FAILURES[failure.code].short}`, tone };
  }
  return { label: name, tone };
}

/**
 * What happened and what to do, for a job's most recent failure.
 * @param {FailureSummary} failure
 */
export function explain(failure) {
  return FAILURES[failure.code].explain;
}

/** @param {Date} time */
function shortTime(time) {
  return time.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
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
 * `left` is the estimated time to finish at the sample's rate.
 * @param {JobSummary} job
 * @param {ProgressSample | undefined} sample
 */
export function progress(job, sample) {
  const done = sample ? Math.max(sample.bytes_received, job.bytes_done) : job.bytes_done;
  const total = sample?.bytes_total ?? job.bytes_total ?? job.artifact?.size;
  const fraction = total ? Math.min(1, done / total) : undefined;
  const text = total ? `${formatBytes(done)} of ${formatBytes(total)}` : formatBytes(done);
  const rate = sample?.bytes_per_second;
  return {
    fraction,
    percent: fraction === undefined ? undefined : `${Math.floor(fraction * 100)}%`,
    text,
    size: total === undefined || total === null ? undefined : formatBytes(total),
    rate: rate === undefined ? undefined : `${formatBytes(rate)}/s`,
    left: rate && total && total > done ? formatDuration((total - done) / rate) : undefined,
  };
}

/**
 * @param {number} seconds
 */
export function formatDuration(seconds) {
  const s = Math.ceil(seconds);
  if (s < 60) return `${s} s`;
  const minutes = Math.ceil(s / 60);
  if (minutes < 60) return `${minutes} min`;
  const hours = Math.floor(minutes / 60);
  return minutes % 60 === 0 ? `${hours} h` : `${hours} h ${minutes % 60} min`;
}

/**
 * The artifact's file name, the last component of its path.
 * @param {JobSummary} job
 */
export function fileName(job) {
  const path = job.artifact?.path;
  return path === undefined ? undefined : path.slice(path.lastIndexOf('/') + 1);
}

/**
 * What to call a job: its file name once published, else its ID.
 * @param {JobSummary} job
 */
export function jobName(job) {
  return fileName(job) ?? job.id;
}
