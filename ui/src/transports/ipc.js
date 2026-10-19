// The desktop transport: `WayfetchClient` over the desktop host's IPC
// commands, for the shared UI in the desktop window (architecture brief
// sections 4.7 and 11.7, ADR-33).
//
// Arguments and answers are the protocol's JSON, as over HTTP, and the
// host's refusals are the protocol's errors, so the views behave the same
// in both shells. There is no sign-in: the window belongs to the person
// using this computer, and the host decides what it may do. Being local,
// this client can also show the host's folder dialog and open a finished
// job's file or folder; each names a job, never a path.
//
// Event subscriptions arrive on a Tauri channel. The host sends a bounded
// number of items ahead of this client's acknowledgements, so a page that
// stops reading never makes the host queue without bound; `null` says the
// stream ended, after which the feed resumes from its cursor as it does
// after a lost HTTP stream.

import { WayfetchError, settingsRequest } from '../client/contract.js';

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
 * @typedef {import('../client/contract.js').CollectMode} CollectMode
 * @typedef {import('../client/contract.js').SettingsChange} SettingsChange
 * @typedef {import('../client/contract.js').RootSummary} RootSummary
 * @typedef {import('../client/contract.js').LinkId} LinkId
 * @typedef {import('../client/contract.js').LinkRef} LinkRef
 * @typedef {import('../client/contract.js').LinksAction} LinksAction
 * @typedef {import('../client/generated/Query.js').Query} Query
 * @typedef {import('../client/generated/ProtocolError.js').ProtocolError} ProtocolError
 * @typedef {import('../client/generated/SubmitRequest.js').SubmitRequest} SubmitRequest
 * @typedef {import('../client/generated/JobCommandRequest.js').JobCommandRequest} JobCommandRequest
 * @typedef {import('../client/generated/RemoveRootRequest.js').RemoveRootRequest} RemoveRootRequest
 * @typedef {import('../client/generated/ChangePluginRequest.js').ChangePluginRequest} ChangePluginRequest
 * @typedef {import('../client/generated/PluginChange.js').PluginChange} PluginChange
 * @typedef {import('../client/generated/CollectRequest.js').CollectRequest} CollectRequest
 * @typedef {import('../client/generated/LinksCommandRequest.js').LinksCommandRequest} LinksCommandRequest
 * @typedef {import('../client/generated/AuthorizeAccountRequest.js').AuthorizeAccountRequest} AuthorizeAccountRequest
 * @typedef {import('../client/generated/AccountCommandRequest.js').AccountCommandRequest} AccountCommandRequest
 * @typedef {import('../client/contract.js').AccountId} AccountId
 * @typedef {import('../client/contract.js').AccountRef} AccountRef
 * @typedef {import('../client/contract.js').AccountAction} AccountAction
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

  /** @param {{ idempotencyKey: string, target: string, items: string[] }} request */
  submit({ idempotencyKey, target, items }) {
    /** @type {SubmitRequest} */
    const request = { idempotency_key: idempotencyKey, target, items };
    return this.call('submit', { request });
  }

  /** @param {{ after?: LinkId | null, pageSize: number }} request */
  listLinks({ after, pageSize }) {
    /** @type {Query} */
    const query = { type: 'list_links', page_size: pageSize };
    if (after) query.after = after;
    return this.call('query', { query });
  }

  /** @param {{ idempotencyKey: string, target: string, text: string, baseUrl?: string | null, mode?: CollectMode, destination?: RootId | null }} request */
  collectLinks({ idempotencyKey, target, text, baseUrl, mode, destination }) {
    /** @type {CollectRequest} */
    const request = { idempotency_key: idempotencyKey, target, text };
    if (baseUrl) request.base_url = baseUrl;
    if (mode) request.mode = mode;
    if (destination) request.destination = destination;
    return this.call('collect_links', { request });
  }

  /**
   * @param {{ idempotencyKey: string, target: string, action: LinksAction, links: LinkRef[], destination?: RootId, account?: AccountId, routing?: StartRouting, media?: MediaChoice }} request
   */
  linksCommand({ idempotencyKey, target, action, links, destination, account, routing, media }) {
    /** @type {LinksCommandRequest} */
    const request = { idempotency_key: idempotencyKey, target, action, links };
    if (destination !== undefined) request.destination = destination;
    if (account !== undefined) request.account = account;
    if (routing !== undefined) request.routing = routing;
    if (media !== undefined) request.media = media;
    return this.call('links_command', { request });
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
   * @param {{ idempotencyKey: string, target: string, action: JobAction, expectedRevision: number, name?: string, repeatProviderOperation?: boolean }} request
   */
  jobCommand(job, { idempotencyKey, target, action, expectedRevision, name, repeatProviderOperation }) {
    /** @type {JobCommandRequest} */
    const request = {
      idempotency_key: idempotencyKey,
      target,
      action,
      expected_revision: expectedRevision,
    };
    if (name !== undefined) request.name = name;
    if (repeatProviderOperation) request.repeat_provider_operation = true;
    return this.call('job_command', { job, request });
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
   * @param {{ idempotencyKey: string, target: string, expectedRevision: number, root: RootId }} request
   */
  removeRoot({ idempotencyKey, target, expectedRevision, root }) {
    /** @type {RemoveRootRequest} */
    const request = {
      idempotency_key: idempotencyKey,
      target,
      expected_revision: expectedRevision,
      root,
    };
    return this.call('remove_root', { request });
  }

  /**
   * Turns a plugin on or off, updates it, or removes the copy Wayfetch
   * installed.
   * @param {{ idempotencyKey: string, target: string, plugin: string, change: PluginChange }} request
   */
  changePlugin({ idempotencyKey, target, plugin, change }) {
    /** @type {ChangePluginRequest} */
    const request = { idempotency_key: idempotencyKey, target, plugin, change };
    return this.call('change_plugin', { request });
  }

  accounts() {
    /** @type {Query} */
    const query = { type: 'accounts' };
    return this.call('query', { query });
  }

  /**
   * @param {{ idempotencyKey: string, target: string, service: string, account?: AccountRef, alias?: string }} request
   */
  authorizeAccount({ idempotencyKey, target, service, account, alias }) {
    /** @type {AuthorizeAccountRequest} */
    const request = { idempotency_key: idempotencyKey, target, service };
    if (account) request.account = account;
    if (alias) request.alias = alias;
    return this.call('authorize_account', { request });
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
   * @param {{ idempotencyKey: string, target: string, action: AccountAction, expectedRevision: number }} request
   */
  accountCommand(account, { idempotencyKey, target, action, expectedRevision }) {
    /** @type {AccountCommandRequest} */
    const request = { idempotency_key: idempotencyKey, target, action, expected_revision: expectedRevision };
    return this.call('account_command', { account, request });
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
