// The browser transport: `WayfetchClient` over the daemon's `/v1` HTTP API
// and server-sent events, authenticated by the owner's session cookie
// (architecture brief sections 8.5 and 11.1).
//
// The CSRF token is held in memory only and re-read from the session after
// a reload. A response that says the session is gone calls
// `onUnauthenticated`, so the shell can ask the user to sign in again.

import { WayfetchError, settingsRequest } from '../client/contract.js';

/**
 * @typedef {import('../client/generated/StartRouting.js').StartRouting} StartRouting
 * @typedef {import('../client/contract.js').WayfetchClient} WayfetchClient
 * @typedef {import('../client/contract.js').EventCursor} EventCursor
 * @typedef {import('../client/contract.js').StreamSink} StreamSink
 * @typedef {import('../client/contract.js').JobId} JobId
 * @typedef {import('../client/contract.js').JobAction} JobAction
 * @typedef {import('../client/contract.js').JobStage} JobStage
 * @typedef {import('../client/contract.js').Permission} Permission
 * @typedef {import('../client/contract.js').RootId} RootId
 * @typedef {import('../client/contract.js').SettingsChange} SettingsChange
 * @typedef {import('../client/contract.js').LinkId} LinkId
 * @typedef {import('../client/contract.js').LinkRef} LinkRef
 * @typedef {import('../client/contract.js').LinksAction} LinksAction
 * @typedef {import('../client/generated/SessionStatus.js').SessionStatus} SessionStatus
 * @typedef {import('../client/generated/SetupRequest.js').SetupRequest} SetupRequest
 * @typedef {import('../client/generated/LoginRequest.js').LoginRequest} LoginRequest
 * @typedef {import('../client/generated/SubmitRequest.js').SubmitRequest} SubmitRequest
 * @typedef {import('../client/generated/JobCommandRequest.js').JobCommandRequest} JobCommandRequest
 * @typedef {import('../client/generated/RemoveRootRequest.js').RemoveRootRequest} RemoveRootRequest
 * @typedef {import('../client/generated/AddRootRequest.js').AddRootRequest} AddRootRequest
 * @typedef {import('../client/generated/CollectRequest.js').CollectRequest} CollectRequest
 * @typedef {import('../client/generated/LinksCommandRequest.js').LinksCommandRequest} LinksCommandRequest
 * @typedef {import('../client/generated/AuthorizeAccountRequest.js').AuthorizeAccountRequest} AuthorizeAccountRequest
 * @typedef {import('../client/generated/AccountCommandRequest.js').AccountCommandRequest} AccountCommandRequest
 * @typedef {import('../client/contract.js').AccountId} AccountId
 * @typedef {import('../client/contract.js').AccountRef} AccountRef
 * @typedef {import('../client/contract.js').AccountAction} AccountAction
 * @typedef {import('../client/contract.js').AuthorizationId} AuthorizationId
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
    /**
     * What the session may do, as the server last said.
     * @type {readonly Permission[]}
     */
    this.permissions = [];
    /** The server's paths are on the server, not where the page is. */
    this.local = false;
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

  /** Ends the session. If this fails, the session may still be live. */
  async logout() {
    await this.request('POST', '/v1/auth/logout', { auth: false });
    this.csrf = null;
    this.permissions = [];
  }

  /**
   * @param {SessionStatus} status
   * @returns {SessionStatus}
   */
  signedIn(status) {
    this.csrf = status.csrf_token ?? null;
    this.permissions = status.permissions ?? [];
    return status;
  }

  // WayfetchClient.

  hello() {
    return this.request('GET', '/v1/capabilities');
  }

  /** @param {{ after?: JobId | null, pageSize: number, stages?: readonly JobStage[] | null }} request */
  listJobs({ after, pageSize, stages }) {
    const query = new URLSearchParams({ page_size: String(pageSize) });
    if (after) query.set('after', after);
    if (stages) query.set('stage', stages.join(','));
    return this.request('GET', `/v1/jobs?${query}`);
  }

  /** @param {JobId} job */
  inspectJob(job) {
    return this.request('GET', `/v1/jobs/${encodeURIComponent(job)}`);
  }

  countJobs() {
    return this.request('GET', '/v1/job-counts');
  }

  /** @param {{ idempotencyKey: string, target: string, items: string[] }} request */
  submit({ idempotencyKey, target, items }) {
    /** @type {SubmitRequest} */
    const body = { idempotency_key: idempotencyKey, target, items };
    return this.request('POST', '/v1/inputs', { body });
  }

  /** @param {{ after?: LinkId | null, pageSize: number }} request */
  listLinks({ after, pageSize }) {
    const query = new URLSearchParams({ page_size: String(pageSize) });
    if (after) query.set('after', after);
    return this.request('GET', `/v1/staging/links?${query}`);
  }

  /** @param {{ idempotencyKey: string, target: string, items: string[], destination?: RootId | null }} request */
  collectLinks({ idempotencyKey, target, items, destination }) {
    /** @type {CollectRequest} */
    const body = { idempotency_key: idempotencyKey, target, items };
    if (destination) body.destination = destination;
    return this.request('POST', '/v1/staging/links', { body });
  }

  /**
   * @param {{ idempotencyKey: string, target: string, action: LinksAction, links: LinkRef[], destination?: RootId, account?: AccountId, routing?: StartRouting }} request
   */
  linksCommand({ idempotencyKey, target, action, links, destination, account, routing }) {
    /** @type {LinksCommandRequest} */
    const body = { idempotency_key: idempotencyKey, target, action, links };
    if (destination !== undefined) body.destination = destination;
    if (account !== undefined) body.account = account;
    if (routing !== undefined) body.routing = routing;
    return this.request('POST', '/v1/staging/commands', { body });
  }

  /** @param {LinkId} link */
  async linkSource(link) {
    const result = await this.request('GET', `/v1/staging/links/${encodeURIComponent(link)}/source`);
    return String(result.url);
  }

  /**
   * @param {JobId} job
   * @param {{ idempotencyKey: string, target: string, action: JobAction, expectedRevision: number, name?: string, repeatProviderOperation?: boolean }} request
   */
  jobCommand(job, { idempotencyKey, target, action, expectedRevision, name, repeatProviderOperation }) {
    /** @type {JobCommandRequest} */
    const body = {
      idempotency_key: idempotencyKey,
      target,
      action,
      expected_revision: expectedRevision,
    };
    if (name !== undefined) body.name = name;
    if (repeatProviderOperation) body.repeat_provider_operation = true;
    return this.request('POST', `/v1/jobs/${encodeURIComponent(job)}/commands`, { body });
  }

  /** @param {JobId} job */
  async jobSource(job) {
    const result = await this.request('GET', `/v1/jobs/${encodeURIComponent(job)}/source`);
    return String(result.url);
  }

  settings() {
    return this.request('GET', '/v1/settings');
  }

  /** @param {SettingsChange} change */
  updateSettings(change) {
    return this.request('PUT', '/v1/settings', { body: settingsRequest(change) });
  }
  /**
   * Stops offering a download folder the user added; its files stay.
   * @param {{ idempotencyKey: string, target: string, expectedRevision: number, root: RootId }} request
   */
  removeRoot({ idempotencyKey, target, expectedRevision, root }) {
    /** @type {RemoveRootRequest} */
    const body = {
      idempotency_key: idempotencyKey,
      target,
      expected_revision: expectedRevision,
      root,
    };
    return this.request('POST', '/v1/roots/remove', { body });
  }

  /**
   * The folders inside `path` on the server, or at its top if `null`.
   * @param {string | null} path
   */
  browseFolders(path) {
    const query = path === null ? '' : `?${new URLSearchParams({ path })}`;
    return this.request('GET', `/v1/folders${query}`);
  }

  /**
   * Adds the server folder at `path` as a download folder.
   * @param {string} path
   */
  addFolder(path) {
    /** @type {AddRootRequest} */
    const body = { path };
    return this.request('POST', '/v1/roots', { body });
  }

  accounts() {
    return this.request('GET', '/v1/accounts');
  }

  /**
   * @param {{ idempotencyKey: string, target: string, service: string, account?: AccountRef, alias?: string }} request
   */
  authorizeAccount({ idempotencyKey, target, service, account, alias }) {
    /** @type {AuthorizeAccountRequest} */
    const body = { idempotency_key: idempotencyKey, target, service };
    if (account) body.account = account;
    if (alias) body.alias = alias;
    return this.request('POST', '/v1/accounts/authorizations', { body });
  }

  /** @param {AuthorizationId} id */
  authorization(id) {
    return this.request('GET', `/v1/accounts/authorizations/${encodeURIComponent(id)}`);
  }

  /** @param {AuthorizationId} id */
  cancelAuthorization(id) {
    return this.request('POST', `/v1/accounts/authorizations/${encodeURIComponent(id)}/cancel`);
  }

  /**
   * @param {AccountId} account
   * @param {{ idempotencyKey: string, target: string, action: AccountAction, expectedRevision: number }} request
   */
  accountCommand(account, { idempotencyKey, target, action, expectedRevision }) {
    /** @type {AccountCommandRequest} */
    const body = { idempotency_key: idempotencyKey, target, action, expected_revision: expectedRevision };
    return this.request('POST', `/v1/accounts/${encodeURIComponent(account)}/commands`, { body });
  }

  /** @param {AccountId} account */
  refreshAccount(account) {
    return this.request('POST', `/v1/accounts/${encodeURIComponent(account)}/refresh`);
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
      retryAt: data.retry_at ?? undefined,
    });
  }
}
