// The contract between the UI's views and a Wayfetch engine (architecture
// brief sections 4.7 and 11.2, ADR-33). Views use only `WayfetchClient`,
// never `fetch` or desktop IPC, so the browser and the desktop shell run
// the same views over different transports.
//
// The data types are `wayfetch-protocol`'s JSON. A transport passes engine
// JSON through unchanged and reports failures as `WayfetchError` with the
// protocol's error code.

/**
 * The protocol version this UI speaks (`ProtocolVersion::CURRENT`).
 * @type {Readonly<ProtocolVersion>}
 */
export const PROTOCOL = Object.freeze({ major: 0, minor: 2 });

/**
 * The wire types, generated from their Rust definitions by ts-rs into
 * `./generated` (see the README's web UI section).
 * @typedef {import('./generated/JobId.js').JobId} JobId
 * @typedef {import('./generated/ProtocolVersion.js').ProtocolVersion} ProtocolVersion
 * @typedef {import('./generated/EventCursor.js').EventCursor} EventCursor
 * @typedef {import('./generated/Hello.js').Hello} Hello
 * @typedef {import('./generated/JobStage.js').JobStage} JobStage
 * @typedef {import('./generated/DisplayState.js').DisplayState} DisplayState
 * @typedef {import('./generated/JobSummary.js').JobSummary} JobSummary
 * @typedef {import('./generated/Page.js').Page<JobSummary>} JobPage
 * @typedef {import('./generated/JobCounts.js').JobCounts} JobCounts
 * @typedef {import('./generated/QueryResult.js').QueryResult} QueryResult
 * @typedef {Omit<Extract<QueryResult, { type: 'job' }>, 'type'>} JobSnapshot
 * @typedef {import('./generated/CommandResult.js').CommandResult} CommandResult
 * @typedef {import('./generated/ProgressSample.js').ProgressSample} ProgressSample
 * @typedef {import('./generated/StreamItem.js').StreamItem} StreamItem
 * @typedef {import('./generated/ErrorCode.js').ErrorCode} ErrorCode
 * @typedef {import('./generated/Action.js').Action} JobAction
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
 *   listJobs: (request: {
 *     after?: JobId | null,
 *     pageSize: number,
 *     stages?: readonly JobStage[] | null,
 *   }) => Promise<JobPage>,
 *   inspectJob: (job: JobId) => Promise<JobSnapshot>,
 *   countJobs: () => Promise<JobCounts>,
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
 * @typedef {ErrorCode | 'unauthenticated' | 'unavailable' | 'incompatible_client'} WayfetchErrorCode
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
   * @param {WayfetchErrorCode} code
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
