// What a job allows and how to describe it: rules every view shares.

/**
 * @typedef {import('./contract.js').JobSummary} JobSummary
 * @typedef {import('./contract.js').StateAction} StateAction
 * @typedef {import('./contract.js').JobStage} JobStage
 * @typedef {import('./contract.js').DisplayState} DisplayState
 * @typedef {import('./contract.js').ProgressSample} ProgressSample
 * @typedef {import('./contract.js').JobCounts} JobCounts
 * @typedef {import('./generated/FailureCode.js').FailureCode} FailureCode
 * @typedef {import('./generated/FailureSummary.js').FailureSummary} FailureSummary
 *
 * How a message or link check reads at a glance: running, waiting on
 * something, quietly stopped, gone wrong, or finished.
 * @typedef {'run' | 'warn' | 'calm' | 'bad' | 'ok'} Tone
 *
 * Which group of states a download is in, for its colour. Each state
 * filter is one; anything else is drawn in a neutral colour.
 * @typedef {'active' | 'waiting' | 'paused' | 'queued' | 'failed' | 'cancelled' | 'done'} Status
 */

/**
 * The commands a job accepts in its current state, mirroring the engine's
 * rules (`JobState::request_*`) so a view offers only those. The engine
 * still decides: a job may change before the command arrives.
 * @param {JobSummary} job
 * @returns {StateAction[]}
 */
export function availableActions(job) {
  /** @type {StateAction[]} */
  const actions = [];
  if (job.stage === 'publishing') return actions;
  if (job.desired === 'running' && job.stage !== 'complete' && job.stage !== 'failed') {
    actions.push('pause');
  }
  if (job.desired === 'paused') actions.push('resume');
  if (job.desired !== 'cancelled' && job.stage !== 'complete') actions.push('cancel');
  // A cancel still stopping keeps its active stage until it settles.
  if (job.stage === 'failed' || job.stage === 'cancelled') actions.push('retry');
  return actions;
}

/**
 * What Retry will do for `job`, in plain words: it continues the data
 * already saved when the server still offers the same file, and otherwise
 * starts over and says why.
 * @param {JobSummary} job
 */
export function retryPlan(job) {
  return job.bytes_done > 0
    ? `Continues from the ${formatBytes(job.bytes_done)} saved when it can`
    : 'Starts the download again';
}

/**
 * Why the job's latest attempt started over from zero, if it did.
 * @param {JobSummary} job
 */
export function restartNote(job) {
  const reason = job.restart_reason;
  return reason ? `Started over from the beginning: ${reason}.` : undefined;
}

/**
 * Whether a job's file can be renamed: only once it is published
 * (`JobState::request_rename`).
 * @param {JobSummary} job
 */
export function canRename(job) {
  return job.stage === 'complete';
}

/**
 * Whether a job can be removed from the list: only once it has ended
 * (`JobState::check_remove`). Its file stays where it is.
 * @param {JobSummary} job
 */
export function canRemove(job) {
  return job.stage === 'complete' || job.stage === 'failed' || job.stage === 'cancelled';
}

/**
 * Each state's plain name and status. Pausing and stopping are on their
 * way to paused and cancelled.
 * @type {Readonly<Record<DisplayState, [string, Status]>>}
 */
const STATES = Object.freeze({
  queued: ['Queued', 'queued'],
  resolving: ['Starting', 'active'],
  transferring: ['Downloading', 'active'],
  verifying: ['Checking file', 'active'],
  publishing: ['Saving', 'active'],
  recovering: ['Recovering', 'active'],
  waiting: ['Waiting', 'waiting'],
  needs_input: ['Needs your answer', 'waiting'],
  paused: ['Paused', 'paused'],
  blocked: ['Blocked', 'waiting'],
  complete: ['Done', 'done'],
  failed: ['Failed', 'failed'],
  cancelled: ['Cancelled', 'cancelled'],
  pausing: ['Pausing…', 'paused'],
  stopping: ['Stopping…', 'cancelled'],
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
      'Wayfetch could not tell whether its last request to the provider went through, so it did not repeat it. Retry asks before repeating it, since the provider may do that work twice.',
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
 * `status` is null for All, which is every state rather than one.
 * @type {readonly { key: string, label: string, status: Status | null, stages: readonly JobStage[] | null }[]}
 */
export const GROUPS = Object.freeze([
  { key: 'all', label: 'All', status: null, stages: null },
  {
    key: 'active',
    label: 'Active',
    status: 'active',
    stages: stages('resolving', 'transferring', 'verifying', 'publishing', 'recovering'),
  },
  { key: 'waiting', label: 'Waiting', status: 'waiting', stages: stages('waiting', 'needs_input', 'blocked') },
  { key: 'paused', label: 'Paused', status: 'paused', stages: stages('paused') },
  { key: 'queued', label: 'Queued', status: 'queued', stages: stages('queued') },
  { key: 'failed', label: 'Failed', status: 'failed', stages: stages('failed') },
  { key: 'cancelled', label: 'Cancelled', status: 'cancelled', stages: stages('cancelled') },
  { key: 'done', label: 'Done', status: 'done', stages: stages('complete') },
]);

/**
 * How many jobs a state filter shows, from the counts per stage.
 * @param {{ stages: readonly JobStage[] | null }} group
 * @param {JobCounts['stages']} counts
 */
export function groupCount(group, counts) {
  const stages = group.stages ?? /** @type {JobStage[]} */ (Object.keys(counts));
  return stages.reduce((sum, stage) => sum + (counts[stage] ?? 0), 0);
}

/**
 * A job's state in plain words, such as "Waiting until 17:42" or
 * "Failed: link expired", and its status. A state this UI does not know
 * keeps the engine's name and no status.
 * @param {JobSummary} job
 * @param {(time: Date) => string} [clock] formats a retry time
 * @returns {{ label: string, status: Status | null }}
 */
export function describe(job, clock = shortTime) {
  /** @type {[string, Status | null]} */
  const [name, status] = STATES[job.display] ?? [String(job.display), null];
  const failure = job.failure;
  if (job.display === 'waiting' && failure?.retry_at !== undefined && failure.retry_at !== null) {
    return { label: `Waiting until ${clock(new Date(failure.retry_at))}`, status };
  }
  const explained = job.display === 'failed' || job.display === 'blocked' || job.display === 'waiting';
  if (failure && explained) {
    return { label: `${name}: ${FAILURES[failure.code].short}`, status };
  }
  return { label: name, status };
}

/**
 * What happened and what to do, for a job's most recent failure.
 * @param {FailureSummary} failure
 */
export function explain(failure) {
  // The engine's redacted context distinguishes rejected authorization from
  // other account refusals and supplies the provider-specific next step.
  if (failure.code === 'invalid_credentials' && failure.message?.trim()) return failure.message;
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
  const ratio = total ? done / total : NaN;
  const fraction = Number.isFinite(ratio) ? Math.min(1, Math.max(0, ratio)) : undefined;
  const text = total ? `${formatBytes(done)} of ${formatBytes(total)}` : formatBytes(done);
  const rate = sample?.bytes_per_second;
  return {
    fraction,
    percent: fraction === undefined ? undefined : percent(fraction),
    text,
    size: total === undefined || total === null ? undefined : formatBytes(total),
    rate: rate === undefined ? undefined : `${formatBytes(rate)}/s`,
    left: rate && total && total > done ? formatDuration((total - done) / rate) : undefined,
  };
}

/**
 * A fraction as a percentage with at most one decimal, rounded down so
 * an unfinished download never reads 100%: 0.9996 is "99.9%", 0.72 "72%".
 * @param {number} fraction between 0 and 1
 */
export function percent(fraction) {
  // The nudge keeps float error (0.57 * 1000 = 569.99…) from losing a tenth.
  const tenths = Math.min(fraction < 1 ? 999 : 1000, Math.floor(fraction * 1000 + 1e-6));
  return `${tenths / 10}%`;
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
 * What to call a job: its published file, the engine's filename hint, or its ID.
 * @param {JobSummary} job
 */
export function jobName(job) {
  return fileName(job) ?? job.name ?? job.id;
}

/**
 * What kind of file each extension names, for formats people commonly
 * download. Others are described by their extension alone.
 * @type {Readonly<Record<string, string>>}
 */
const KINDS = Object.freeze({
  zip: 'ZIP archive',
  '7z': '7-Zip archive',
  rar: 'RAR archive',
  tar: 'Tar archive',
  'tar.gz': 'Gzip-compressed tar archive',
  tgz: 'Gzip-compressed tar archive',
  'tar.xz': 'XZ-compressed tar archive',
  txz: 'XZ-compressed tar archive',
  'tar.bz2': 'Bzip2-compressed tar archive',
  'tar.zst': 'Zstandard-compressed tar archive',
  gz: 'Gzip-compressed file',
  xz: 'XZ-compressed file',
  bz2: 'Bzip2-compressed file',
  zst: 'Zstandard-compressed file',
  iso: 'Disc image',
  img: 'Disk image',
  dmg: 'macOS disk image',
  exe: 'Windows program',
  msi: 'Windows installer',
  deb: 'Debian package',
  rpm: 'RPM package',
  apk: 'Android package',
  appimage: 'Linux AppImage',
  mp4: 'MP4 video',
  m4v: 'MP4 video',
  mkv: 'Matroska video',
  webm: 'WebM video',
  avi: 'AVI video',
  mov: 'QuickTime video',
  mp3: 'MP3 audio',
  m4a: 'MPEG-4 audio',
  aac: 'AAC audio',
  flac: 'FLAC audio',
  ogg: 'Ogg audio',
  opus: 'Opus audio',
  wav: 'WAV audio',
  jpg: 'JPEG image',
  jpeg: 'JPEG image',
  png: 'PNG image',
  gif: 'GIF image',
  webp: 'WebP image',
  avif: 'AVIF image',
  svg: 'SVG image',
  pdf: 'PDF document',
  epub: 'EPUB e-book',
  txt: 'Text file',
  csv: 'CSV table',
  json: 'JSON file',
  docx: 'Word document',
  xlsx: 'Excel workbook',
  pptx: 'PowerPoint presentation',
  torrent: 'Torrent file',
});

/**
 * What kind of file `name` is, judged by its extension: the engine
 * records no media type, and the name is what the user's programs go by.
 * @param {string} name
 */
export function fileKind(name) {
  const lower = name.toLowerCase();
  const parts = lower.split('.');
  if (parts.length < 2 || (parts.length === 2 && parts[0] === '')) return 'File with no extension';
  const double = parts.slice(-2).join('.');
  const extension = /** @type {string} */ (parts.at(-1));
  if (parts.length > 2 && Object.hasOwn(KINDS, double)) return KINDS[double];
  if (Object.hasOwn(KINDS, extension)) return KINDS[extension];
  return /^[a-z0-9]{1,10}$/.test(extension) ? `${extension.toUpperCase()} file` : 'File';
}

/**
 * A link as it can be shown on screen: scheme, host, and path. Credentials,
 * the query, and the fragment are left out, since they can carry a signed
 * token; `hidden` says whether any were. `null` if it is not a URL.
 * @param {string} link
 * @returns {{ text: string, hidden: boolean } | null}
 */
export function displayLink(link) {
  let url;
  try {
    url = new URL(link);
  } catch {
    return null;
  }
  const hidden = url.username !== '' || url.password !== '' || url.search !== '' || url.hash !== '';
  return { text: `${url.protocol}//${url.host}${url.pathname}`, hidden };
}
