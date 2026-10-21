// The desktop transport: `WayfetchClient` over the desktop host's IPC
// commands (ADR-33).
//
// Arguments, answers, and errors are the protocol's JSON, as over HTTP.
// There is no sign-in: the window belongs to the local user. Events arrive
// on a Tauri channel with acknowledgements, so a page that stops reading
// never makes the host queue without bound.

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
 * @typedef {import('../client/contract.js').StreamItem} StreamItem
 * @typedef {import('../client/contract.js').JobId} JobId
 * @typedef {import('../client/contract.js').JobAction} JobAction
 * @typedef {import('../client/contract.js').JobStage} JobStage
 * @typedef {import('../client/contract.js').Permission} Permission
 * @typedef {import('../client/contract.js').RootId} RootId
 * @typedef {import('../client/contract.js').SettingsChange} SettingsChange
 * @typedef {import('../client/contract.js').ExtractChange} ExtractChange
 * @typedef {import('../client/contract.js').RootSummary} RootSummary
 * @typedef {import('../client/contract.js').LinkId} LinkId
 * @typedef {import('../client/generated/Query.js').Query} Query
 * @typedef {import('../client/generated/ProtocolError.js').ProtocolError} ProtocolError
 * @typedef {import('../client/generated/SubmitRequest.js').SubmitRequest} SubmitRequest
 * @typedef {import('../client/generated/RemoveRootRequest.js').RemoveRootRequest} RemoveRootRequest
 * @typedef {import('../client/generated/ChangePluginRequest.js').ChangePluginRequest} ChangePluginRequest
 * @typedef {import('../client/generated/PluginChange.js').PluginChange} PluginChange
 * @typedef {import('../client/generated/CollectRequest.js').CollectRequest} CollectRequest
 * @typedef {import('../client/generated/LinksCommandRequest.js').LinksCommandRequest} LinksCommandRequest
 * @typedef {import('../client/generated/AuthorizeAccountRequest.js').AuthorizeAccountRequest} AuthorizeAccountRequest
 * @typedef {import('../client/generated/AccountCommandRequest.js').AccountCommandRequest} AccountCommandRequest
 * @typedef {import('../client/contract.js').AccountId} AccountId
 * @typedef {import('../client/contract.js').AuthorizationId} AuthorizationId
 */

/**
 * What the host grants the window: the local owner's permissions.
 * @type {readonly Permission[]}
 */
const OWNER = Object.freeze([
  'read', 'submit', 'control', 'sources', 'accounts', 'provider_downloads', 'delete_files',
]);

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
      retryAt: refused.retry_at ?? undefined,
    });
  }
  return new WayfetchError('unavailable', `The desktop engine did not answer: ${String(error)}`);
}

/** @implements {WayfetchClient} */
export class DesktopIpcClient {
  /** @param {TauriCore} [core] */
  constructor(core = tauriCore()) {
    this.core = core;
    this.permissions = OWNER;
    /** The engine runs in this app, on this computer. */
    this.local = true;
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

  /** @param {Args<'submit'>[0]} request */
  submit(request) {
    return this.call('submit', { request: submitRequest(request) });
  }

  /** @param {{ after?: LinkId | null, pageSize: number }} request */
  listLinks({ after, pageSize }) {
    /** @type {Query} */
    const query = { type: 'list_links', page_size: pageSize };
    if (after) query.after = after;
    return this.call('query', { query });
  }

  /** @param {Args<'collectLinks'>[0]} request */
  collectLinks(request) {
    return this.call('collect_links', { request: collectRequest(request) });
  }

  /**
   * @param {Args<'linksCommand'>[0]} request
   */
  linksCommand(request) {
    return this.call('links_command', { request: linksCommandRequest(request) });
  }

  /** @param {LinkId} link */
  async linkSource(link) {
    /** @type {Query} */
    const query = { type: 'link_source', link };
    const result = await this.call('query', { query });
    return String(result.url);
  }

  /**
   * @param {JobId} job
   * @param {import('../client/contract.js').JobChange} change
   */
  jobCommand(job, change) {
    return this.call('job_command', { job, request: jobCommandRequest(change) });
  }

  /**
   * Unpacks a downloaded archive, with its password if it needs one.
   * @param {JobId} job
   * @param {ExtractChange} change
   */
  extract(job, change) {
    return this.call('extract', { job, request: extractRequest(change) });
  }

  /** @param {JobId} job */
  async jobSource(job) {
    /** @type {Query} */
    const query = { type: 'job_source', job };
    const result = await this.call('query', { query });
    return String(result.url);
  }

  settings() {
    /** @type {Query} */
    const query = { type: 'settings' };
    return this.call('query', { query });
  }

  /** @param {SettingsChange} change */
  updateSettings(change) {
    return this.call('update_settings', { request: settingsRequest(change) });
  }
  /**
   * Stops offering a download folder the user added; its files stay.
   * @param {Args<'removeRoot'>[0]} request
   */
  removeRoot(request) {
    return this.call('remove_root', { request: removeRootRequest(request) });
  }

  /**
   * Turns a plugin on or off, updates it, or removes the copy Wayfetch
   * installed.
   * @param {Args<'changePlugin'>[0]} request
   */
  changePlugin(request) {
    return this.call('change_plugin', { request: changePluginRequest(request) });
  }

  accounts() {
    /** @type {Query} */
    const query = { type: 'accounts' };
    return this.call('query', { query });
  }

  updates() {
    /** @type {Query} */
    const query = { type: 'updates' };
    return this.call('query', { query });
  }

  checkForUpdates() {
    /** @type {Query} */
    const query = { type: 'updates', check: true };
    return this.call('query', { query });
  }

  /**
   * Installs the newer release the app downloaded, which asks for the
   * administrator's password, then restarts into it.
   * @returns {Promise<void>}
   */
  restartToUpdate() {
    return this.call('restart_to_update');
  }

  /**
   * @param {Args<'authorizeAccount'>[0]} request
   */
  authorizeAccount(request) {
    return this.call('authorize_account', { request: authorizeAccountRequest(request) });
  }

  /** @param {AuthorizationId} id */
  authorization(id) {
    /** @type {Query} */
    const query = { type: 'authorization', id };
    return this.call('query', { query });
  }

  /** @param {AuthorizationId} id */
  cancelAuthorization(id) {
    return this.call('cancel_authorization', { authorization: id });
  }

  /**
   * @param {AccountId} account
   * @param {Args<'accountCommand'>[1]} request
   */
  accountCommand(account, request) {
    return this.call('account_command', { account, request: accountCommandRequest(request) });
  }

  /** @param {AccountId} account */
  refreshAccount(account) {
    return this.call('refresh_account', { account });
  }

  /**
   * Shows the host's folder dialog; the folder chosen becomes a permitted
   * destination. `null` if the dialog was cancelled.
   * @returns {Promise<RootSummary | null>}
   */
  chooseDownloadFolder() {
    return this.call('choose_download_folder');
  }

  /**
   * Opens a finished job's file with this computer's default application.
   * @param {JobId} job
   * @returns {Promise<void>}
   */
  openFile(job) {
    return this.call('open_file', { job });
  }

  /**
   * Shows a finished job's file in this computer's file manager.
   * @param {JobId} job
   * @returns {Promise<void>}
   */
  revealFile(job) {
    return this.call('reveal_file', { job });
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
