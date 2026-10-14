// The contract between the UI's views and a Wayfetch engine (architecture
// brief sections 4.7 and 11.2, ADR-33). Views use only `WayfetchClient`,
// never `fetch` or desktop IPC, so the browser and the desktop shell run
// the same views over different transports.
//
// The data types mirror `wayfetch-protocol`'s JSON. A transport passes
// engine JSON through unchanged and reports failures as `WayfetchError`
// with the protocol's error code.

/**
 * The protocol version this UI speaks (`ProtocolVersion::CURRENT`).
 * @type {Readonly<ProtocolVersion>}
 */
export const PROTOCOL = Object.freeze({ major: 0, minor: 1 });

/**
 * @typedef {`job-${number}`} JobId
 * @typedef {{ major: number, minor: number }} ProtocolVersion
 * @typedef {{ instance: string, position: number }} EventCursor
 * @typedef {{ max_page_size: number, max_submit_items: number, max_url_bytes: number }} TransportLimits
 * @typedef {{
 *   instance: string,
 *   boot_epoch: number,
 *   protocol: ProtocolVersion,
 *   head: EventCursor,
 *   limits: TransportLimits,
 * }} Hello
 * @typedef {'queued' | 'resolving' | 'transferring' | 'verifying' | 'publishing' | 'recovering'
 *   | 'waiting' | 'needs_input' | 'paused' | 'blocked' | 'complete' | 'failed' | 'cancelled'} JobStage
 * @typedef {'running' | 'paused' | 'cancelled'} DesiredState
 * @typedef {JobStage | 'pausing' | 'stopping'} DisplayState
 * @typedef {{ code: string, retry_at?: number, message?: string }} FailureSummary
 * @typedef {{ root: string, path: string, size: number }} ArtifactSummary
 * @typedef {{
 *   id: JobId,
 *   package: string,
 *   stage: JobStage,
 *   desired: DesiredState,
 *   display: DisplayState,
 *   revision: number,
 *   generation: number,
 *   bytes_done: number,
 *   bytes_total?: number,
 *   failure?: FailureSummary,
 *   artifact?: ArtifactSummary,
 * }} JobSummary
 * @typedef {{ items: JobSummary[], next_after?: JobId, as_of: EventCursor }} JobPage
 * @typedef {{ job: JobSummary, as_of: EventCursor }} JobSnapshot
 * @typedef {{ type: 'submitted', batch: string, accepted: number }
 *   | { type: 'job_updated', job: JobId, revision: number, changed: boolean }} CommandResult
 * @typedef {{ type: 'batch_accepted', batch: string, accepted: number }
 *   | { type: 'job_added', job: JobSummary }
 *   | { type: 'job_changed', job: JobSummary }} EventKind
 * @typedef {{
 *   boot_epoch: number,
 *   job: JobId,
 *   generation: number,
 *   bytes_received: number,
 *   bytes_total?: number,
 *   bytes_per_second: number,
 * }} ProgressSample
 * @typedef {'unknown_instance' | 'cursor_expired' | 'cursor_ahead' | 'lagged'} ResyncReason
 * @typedef {({ type: 'hello' } & Hello)
 *   | { type: 'event', cursor: EventCursor, kind: EventKind }
 *   | ({ type: 'progress' } & ProgressSample)
 *   | { type: 'resync', reason: ResyncReason }} StreamItem
 * @typedef {'pause' | 'resume' | 'cancel'} JobAction
 */

/**
 * Receives one subscription's items. `end` is called once, when the stream
 * ends for any reason other than `close()`; the subscriber then decides
 * whether and how to subscribe again.
 * @typedef {{ item: (item: StreamItem) => void, end: () => void }} StreamSink
 * @typedef {{ close: () => void }} Subscription
 */

/**
 * What views need from an engine. Every method rejects with a
 * `WayfetchError`.
 * @typedef {{
 *   hello: () => Promise<Hello>,
 *   listJobs: (request: { after?: JobId | null, pageSize: number }) => Promise<JobPage>,
 *   inspectJob: (job: JobId) => Promise<JobSnapshot>,
 *   submit: (request: { idempotencyKey: string, target: string, items: string[] }) => Promise<CommandResult>,
 *   jobCommand: (job: JobId, request: {
 *     idempotencyKey: string,
 *     target: string,
 *     action: JobAction,
 *     expectedRevision: number,
 *   }) => Promise<CommandResult>,
 *   subscribe: (after: EventCursor | null, sink: StreamSink) => Subscription,
 * }} WayfetchClient
 */

/**
 * A failed request. `code` is the protocol's `ErrorCode`, or one of the
 * client-side codes:
 *
 * - `unauthenticated`: the transport has no valid credentials (for a
 *   browser, its session ended); sign in again.
 * - `unavailable`: the engine could not be reached.
 * - `incompatible_client`: this UI cannot talk to the engine's protocol.
 */
export class WayfetchError extends Error {
  /**
   * @param {string} code
   * @param {string} message
   * @param {{ currentRevision?: number, retryAfter?: number }} [details]
   */
  constructor(code, message, details = {}) {
    super(message);
    this.name = 'WayfetchError';
    this.code = code;
    /** The record's revision, for `revision_conflict`. */
    this.currentRevision = details.currentRevision;
    /** Seconds to wait before trying again, when the server said so. */
    this.retryAfter = details.retryAfter;
  }
}

/**
 * Whether an engine speaking `engine` serves this UI: the same major
 * version, and nothing newer than it provides (`ProtocolVersion::serves`).
 * @param {ProtocolVersion} engine
 */
export function serves(engine) {
  return engine.major === PROTOCOL.major && PROTOCOL.minor <= engine.minor;
}

/** A fresh idempotency key. */
export function idempotencyKey() {
  return crypto.randomUUID();
}
