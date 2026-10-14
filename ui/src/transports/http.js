// The browser transport: `WayfetchClient` over the daemon's `/v1` HTTP API
// and server-sent events, authenticated by the owner's session cookie
// (architecture brief sections 8.5 and 11.1).
//
// The CSRF token is held in memory only and re-read from the session after
// a reload. A response that says the session is gone calls
// `onUnauthenticated`, so the shell can ask the user to sign in again.

import { WayfetchError } from '../client/contract.js';

/**
 * @typedef {import('../client/contract.js').WayfetchClient} WayfetchClient
 * @typedef {import('../client/contract.js').EventCursor} EventCursor
 * @typedef {import('../client/contract.js').StreamSink} StreamSink
 * @typedef {import('../client/contract.js').JobId} JobId
 * @typedef {import('../client/contract.js').JobAction} JobAction
 * @typedef {import('../client/generated/SessionStatus.js').SessionStatus} SessionStatus
 * @typedef {import('../client/generated/SetupRequest.js').SetupRequest} SetupRequest
 * @typedef {import('../client/generated/LoginRequest.js').LoginRequest} LoginRequest
 * @typedef {import('../client/generated/SubmitRequest.js').SubmitRequest} SubmitRequest
 * @typedef {import('../client/generated/JobCommandRequest.js').JobCommandRequest} JobCommandRequest
 */

/** Server-sent event types, as `wayfetch-api` names them. */
const STREAM_EVENTS = ['hello', 'event', 'progress', 'resync'];

/** @implements {WayfetchClient} */
export class HttpClient {
  /**
   * @param {{ onUnauthenticated?: () => void }} [options]
   */
  constructor(options = {}) {
    this.onUnauthenticated = options.onUnauthenticated ?? (() => {});
    /** @type {string | null} */
    this.csrf = null;
  }

  // Browser sign-in.

  /**
   * The browser's session, or null if the server has no browser access.
   * @returns {Promise<SessionStatus | null>}
   */
  async session() {
    try {
      return this.signedIn(await this.request('GET', '/v1/auth/session', { auth: false }));
    } catch (error) {
      if (error instanceof WayfetchError && error.code === 'not_found') return null;
      throw error;
    }
  }

  /**
   * Creates the owner account and signs in.
   * @param {{ setupSecret: string, username: string, password: string }} request
   */
  async setup({ setupSecret, username, password }) {
    /** @type {SetupRequest} */
    const body = { setup_secret: setupSecret, username, password };
    return this.signedIn(await this.request('POST', '/v1/auth/setup', { body, auth: false }));
  }

  /**
   * @param {{ username: string, password: string }} request
   */
  async login({ username, password }) {
    /** @type {LoginRequest} */
    const body = { username, password };
    return this.signedIn(await this.request('POST', '/v1/auth/login', { body, auth: false }));
  }

  async logout() {
    try {
      await this.request('POST', '/v1/auth/logout', { auth: false });
    } finally {
      this.csrf = null;
    }
  }

  /**
   * @param {SessionStatus} status
   * @returns {SessionStatus}
   */
  signedIn(status) {
    this.csrf = status.csrf_token ?? null;
    return status;
  }

  // WayfetchClient.

  hello() {
    return this.request('GET', '/v1/capabilities');
  }

  /** @param {{ after?: JobId | null, pageSize: number }} request */
  listJobs({ after, pageSize }) {
    const query = new URLSearchParams({ page_size: String(pageSize) });
    if (after) query.set('after', after);
    return this.request('GET', `/v1/jobs?${query}`);
  }

  /** @param {JobId} job */
  inspectJob(job) {
    return this.request('GET', `/v1/jobs/${encodeURIComponent(job)}`);
  }

  /** @param {{ idempotencyKey: string, target: string, items: string[] }} request */
  submit({ idempotencyKey, target, items }) {
    /** @type {SubmitRequest} */
    const body = { idempotency_key: idempotencyKey, target, items };
    return this.request('POST', '/v1/inputs', { body });
  }

  /**
   * @param {JobId} job
   * @param {{ idempotencyKey: string, target: string, action: JobAction, expectedRevision: number }} request
   */
  jobCommand(job, { idempotencyKey, target, action, expectedRevision }) {
    /** @type {JobCommandRequest} */
    const body = {
      idempotency_key: idempotencyKey,
      target,
      action,
      expected_revision: expectedRevision,
    };
    return this.request('POST', `/v1/jobs/${encodeURIComponent(job)}/commands`, { body });
  }

  /**
   * Follows `/v1/events` after `after`. The browser's own reconnection is
   * not used: the first error ends the subscription, and the caller
   * subscribes again from its cursor after checking the session.
   * @param {EventCursor | null} after
   * @param {StreamSink} sink
   */
  subscribe(after, sink) {
    const query = after ? `?after=${encodeURIComponent(`${after.instance}:${after.position}`)}` : '';
    const source = new EventSource(`/v1/events${query}`);
    let open = true;
    const close = () => {
      open = false;
      source.close();
    };
    for (const type of STREAM_EVENTS) {
      source.addEventListener(type, (event) => {
        if (!open) return;
        const item = JSON.parse(/** @type {MessageEvent<string>} */ (event).data);
        // A resync ends the stream; never let the browser reconnect it.
        if (item.type === 'resync') close();
        sink.item(item);
      });
    }
    source.addEventListener('error', () => {
      if (!open) return;
      close();
      sink.end();
    });
    return { close };
  }

  /**
   * One request. `auth: false` marks the sign-in routes, whose refusals
   * are about the attempt, not an ended session.
   * @param {string} method
   * @param {string} path
   * @param {{ body?: unknown, auth?: boolean }} [options]
   * @returns {Promise<any>}
   */
  async request(method, path, { body, auth = true } = {}) {
    /** @type {Record<string, string>} */
    const headers = { Accept: 'application/json' };
    if (body !== undefined) headers['Content-Type'] = 'application/json';
    if (method !== 'GET' && this.csrf) headers['X-CSRF-Token'] = this.csrf;
    let response;
    try {
      response = await fetch(path, {
        method,
        headers,
        body: body === undefined ? undefined : JSON.stringify(body),
        credentials: 'same-origin',
        cache: 'no-store',
        redirect: 'error',
      });
    } catch {
      throw new WayfetchError('unavailable', 'The server cannot be reached.');
    }
    if (response.status === 204) return undefined;
    const data = await response.json().catch(() => undefined);
    if (response.ok && data !== undefined) return data;
    if (response.status === 401 && auth) {
      this.csrf = null;
      this.onUnauthenticated();
      throw new WayfetchError('unauthenticated', 'Your session has ended. Sign in again.');
    }
    if (!data || typeof data.code !== 'string') {
      throw new WayfetchError('unavailable', `The server answered ${response.status}.`);
    }
    const retryAfter = Number(response.headers.get('Retry-After')) || undefined;
    throw new WayfetchError(data.code, String(data.message ?? data.code), {
      currentRevision: data.current_revision,
      retryAfter,
    });
  }
}
