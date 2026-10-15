// The desktop transport: `WayfetchClient` over the desktop host's IPC
// commands, for the shared UI in the desktop window (architecture brief
// sections 4.7 and 11.7, ADR-33).
//
// Arguments and answers are the protocol's JSON, as over HTTP, and the
// host's refusals are the protocol's errors, so the views behave the same
// in both shells. There is no sign-in: the window belongs to the person
// using this computer, and the host decides what it may do.
//
// Event subscriptions arrive on a Tauri channel. The host sends a bounded
// number of items ahead of this client's acknowledgements, so a page that
// stops reading never makes the host queue without bound; `null` says the
// stream ended, after which the feed resumes from its cursor as it does
// after a lost HTTP stream.

import { WayfetchError } from '../client/contract.js';

/**
 * @typedef {import('../client/contract.js').WayfetchClient} WayfetchClient
 * @typedef {import('../client/contract.js').EventCursor} EventCursor
 * @typedef {import('../client/contract.js').StreamSink} StreamSink
 * @typedef {import('../client/contract.js').StreamItem} StreamItem
 * @typedef {import('../client/contract.js').JobId} JobId
 * @typedef {import('../client/contract.js').JobAction} JobAction
 * @typedef {import('../client/contract.js').JobStage} JobStage
 * @typedef {import('../client/generated/Query.js').Query} Query
 * @typedef {import('../client/generated/ProtocolError.js').ProtocolError} ProtocolError
 * @typedef {import('../client/generated/SubmitRequest.js').SubmitRequest} SubmitRequest
 * @typedef {import('../client/generated/JobCommandRequest.js').JobCommandRequest} JobCommandRequest
 */

/**
 * The parts of Tauri's global API (`window.__TAURI__.core`) this client
 * uses.
 * @typedef {{ onmessage: (message: any) => void }} IpcChannel
 * @typedef {{
 *   invoke: (command: string, args?: Record<string, unknown>) => Promise<any>,
 *   Channel: new () => IpcChannel,
 * }} TauriCore
 */

/** Items acknowledged at once; a quarter of what the host sends ahead. */
export const ACK_EVERY = 16;

/** Tauri's core API, which the desktop host injects into its window. */
function tauriCore() {
  const core = /** @type {{ __TAURI__?: { core?: TauriCore } }} */ (/** @type {unknown} */ (window))
    .__TAURI__?.core;
  if (!core) throw new WayfetchError('unavailable', 'This page is not running in the desktop app.');
  return core;
}

/**
 * The error a refused command reports: the protocol's own, or
 * `unavailable` when the host could not run the command at all.
 * @param {unknown} error
 */
function failure(error) {
  if (error && typeof error === 'object' && typeof (/** @type {any} */ (error).code) === 'string') {
    const refused = /** @type {ProtocolError} */ (error);
    return new WayfetchError(refused.code, String(refused.message ?? refused.code), {
      currentRevision: refused.current_revision ?? undefined,
    });
  }
  return new WayfetchError('unavailable', `The desktop engine did not answer: ${String(error)}`);
}

/** @implements {WayfetchClient} */
export class DesktopIpcClient {
  /** @param {TauriCore} [core] */
  constructor(core = tauriCore()) {
    this.core = core;
  }

  hello() {
    return this.call('hello');
  }

  /** @param {{ after?: JobId | null, pageSize: number, stages?: readonly JobStage[] | null }} request */
  listJobs({ after, pageSize, stages }) {
    /** @type {Query} */
    const query = { type: 'list_jobs', page_size: pageSize };
    if (after) query.after = after;
    if (stages) query.filter = { stages: [...stages] };
    return this.call('query', { query });
  }

  /** @param {JobId} job */
  inspectJob(job) {
    /** @type {Query} */
    const query = { type: 'inspect_job', job };
    return this.call('query', { query });
  }

  countJobs() {
    /** @type {Query} */
    const query = { type: 'count_jobs' };
    return this.call('query', { query });
  }

  /** @param {{ idempotencyKey: string, target: string, items: string[] }} request */
  submit({ idempotencyKey, target, items }) {
    /** @type {SubmitRequest} */
    const request = { idempotency_key: idempotencyKey, target, items };
    return this.call('submit', { request });
  }

  /**
   * @param {JobId} job
   * @param {{ idempotencyKey: string, target: string, action: JobAction, expectedRevision: number }} request
   */
  jobCommand(job, { idempotencyKey, target, action, expectedRevision }) {
    /** @type {JobCommandRequest} */
    const request = {
      idempotency_key: idempotencyKey,
      target,
      action,
      expected_revision: expectedRevision,
    };
    return this.call('job_command', { job, request });
  }

  /**
   * Follows durable events after `after`. A refused subscription, such as
   * one over the host's limit, ends like a lost stream.
   * @param {EventCursor | null} after
   * @param {StreamSink} sink
   */
  subscribe(after, sink) {
    const channel = new this.core.Channel();
    let open = true;
    /** @type {number | null} */
    let id = null;
    let received = 0;
    let acknowledged = 0;
    const acknowledge = () => {
      if (!open || id === null || received - acknowledged < ACK_EVERY) return;
      acknowledged = received;
      // A lost acknowledgement only delays delivery; the host ends a
      // stream that waits too long, and the feed resumes from its cursor.
      this.core.invoke('acknowledge', { subscription: id, received }).catch(() => {});
    };
    const close = () => {
      if (!open) return;
      open = false;
      if (id !== null) this.core.invoke('unsubscribe', { subscription: id }).catch(() => {});
    };
    channel.onmessage = (/** @type {StreamItem | null} */ message) => {
      if (!open) return;
      if (message === null) {
        open = false;
        sink.end();
        return;
      }
      received += 1;
      // A resync ends the stream on both sides.
      if (message.type === 'resync') close();
      sink.item(message);
      acknowledge();
    };
    this.core.invoke('subscribe', { after, channel }).then(
      (/** @type {number} */ subscription) => {
        id = subscription;
        if (open) acknowledge();
        else this.core.invoke('unsubscribe', { subscription }).catch(() => {});
      },
      () => {
        if (!open) return;
        open = false;
        sink.end();
      },
    );
    return { close };
  }

  /**
   * One command; rejects with a `WayfetchError`.
   * @param {string} command
   * @param {Record<string, unknown>} [args]
   * @returns {Promise<any>}
   */
  async call(command, args) {
    try {
      return await this.core.invoke(command, args);
    } catch (error) {
      throw failure(error);
    }
  }
}
