// The browser transport: `WayfetchClient` over the daemon's `/v1` HTTP API
// and server-sent events, authenticated by the owner's session cookie.
//
// The CSRF token is held in memory only and re-read from the session after
// a reload. A response that says the session is gone calls
// `onUnauthenticated`, so the shell can ask the user to sign in again.

import {
  WayfetchError,
  accountCommandRequest,
  authorizeAccountRequest,
  changePluginRequest,
  collectRequest,
  extractRequest,
  jobCommandRequest,
  linksCommandRequest,
  removeRootRequest,
  settingsRequest,
  submitRequest,
} from '../client/contract.js';

/**
 * @template {keyof WayfetchClient} M
 * @typedef {import('../client/contract.js').Args<M>} Args
 */

/**
 * @typedef {import('../client/generated/StartRouting.js').StartRouting} StartRouting
 * @typedef {import('../client/generated/MediaChoice.js').MediaChoice} MediaChoice
 * @typedef {import('../client/contract.js').WayfetchClient} WayfetchClient
 * @typedef {import('../client/contract.js').EventCursor} EventCursor
 * @typedef {import('../client/contract.js').StreamSink} StreamSink
 * @typedef {import('../client/contract.js').JobId} JobId
 * @typedef {import('../client/contract.js').JobAction} JobAction
 * @typedef {import('../client/contract.js').JobStage} JobStage
 * @typedef {import('../client/contract.js').Permission} Permission
 * @typedef {import('../client/contract.js').RootId} RootId
 * @typedef {import('../client/contract.js').SettingsChange} SettingsChange
 * @typedef {import('../client/contract.js').ExtractChange} ExtractChange
 * @typedef {import('../client/contract.js').LinkId} LinkId
 * @typedef {import('../client/generated/SessionStatus.js').SessionStatus} SessionStatus
 * @typedef {import('../client/generated/SetupRequest.js').SetupRequest} SetupRequest
 * @typedef {import('../client/generated/LoginRequest.js').LoginRequest} LoginRequest
 * @typedef {import('../client/generated/SubmitRequest.js').SubmitRequest} SubmitRequest
 * @typedef {import('../client/generated/RemoveRootRequest.js').RemoveRootRequest} RemoveRootRequest
 * @typedef {import('../client/generated/ChangePluginRequest.js').ChangePluginRequest} ChangePluginRequest
 * @typedef {import('../client/generated/PluginChange.js').PluginChange} PluginChange
 * @typedef {import('../client/generated/AddRootRequest.js').AddRootRequest} AddRootRequest
 * @typedef {import('../client/generated/CollectRequest.js').CollectRequest} CollectRequest
 * @typedef {import('../client/generated/LinksCommandRequest.js').LinksCommandRequest} LinksCommandRequest
 * @typedef {import('../client/generated/AuthorizeAccountRequest.js').AuthorizeAccountRequest} AuthorizeAccountRequest
 * @typedef {import('../client/generated/AccountCommandRequest.js').AccountCommandRequest} AccountCommandRequest
 * @typedef {import('../client/contract.js').AccountId} AccountId
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

  /** @param {Args<'submit'>[0]} request */
  submit(request) {
    return this.request('POST', '/v1/inputs', { body: submitRequest(request) });
  }

  /** @param {{ after?: LinkId | null, pageSize: number }} request */
  listLinks({ after, pageSize }) {
    const query = new URLSearchParams({ page_size: String(pageSize) });
    if (after) query.set('after', after);
    return this.request('GET', `/v1/staging/links?${query}`);
  }

  /** @param {Args<'collectLinks'>[0]} request */
  collectLinks(request) {
    return this.request('POST', '/v1/staging/links', { body: collectRequest(request) });
  }

  /**
   * @param {Args<'linksCommand'>[0]} request
   */
  linksCommand(request) {
    return this.request('POST', '/v1/staging/commands', { body: linksCommandRequest(request) });
  }

  /** @param {LinkId} link */
  async linkSource(link) {
    const result = await this.request('GET', `/v1/staging/links/${encodeURIComponent(link)}/source`);
    return String(result.url);
  }

  /**
   * @param {JobId} job
   * @param {import('../client/contract.js').JobChange} change
   */
  jobCommand(job, change) {
    return this.request('POST', `/v1/jobs/${encodeURIComponent(job)}/commands`, { body: jobCommandRequest(change) });
  }

  /**
   * Unpacks a downloaded archive, with its password if it needs one.
   * @param {JobId} job
   * @param {ExtractChange} change
   */
  extract(job, change) {
    return this.request('POST', `/v1/jobs/${encodeURIComponent(job)}/extract`, { body: extractRequest(change) });
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
   * @param {Args<'removeRoot'>[0]} request
   */
  removeRoot(request) {
    return this.request('POST', '/v1/roots/remove', { body: removeRootRequest(request) });
  }

  /**
   * Turns a plugin on or off, updates it, or removes the copy Wayfetch
   * installed.
   * @param {Args<'changePlugin'>[0]} request
   */
  changePlugin(request) {
    return this.request('POST', '/v1/plugins/change', { body: changePluginRequest(request) });
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

  /** Asks the server whether a newer release is published. */
  checkForUpdates() {
    return this.request('POST', '/v1/updates/check');
  }

  /**
   * @param {Args<'authorizeAccount'>[0]} request
   */
  authorizeAccount(request) {
    return this.request('POST', '/v1/accounts/authorizations', { body: authorizeAccountRequest(request) });
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
   * @param {Args<'accountCommand'>[1]} request
   */
  accountCommand(account, request) {
    return this.request('POST', `/v1/accounts/${encodeURIComponent(account)}/commands`, { body: accountCommandRequest(request) });
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
