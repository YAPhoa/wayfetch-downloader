// The contract between the UI's views and a Wayfetch engine (ADR-33).
// Views use only `WayfetchClient`, never `fetch` or desktop IPC, so the
// browser and the desktop shell run the same views over different
// transports.
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
 * `./generated` (see ui/README.md).
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
 * @typedef {import('./generated/Permission.js').Permission} Permission
 * @typedef {import('./generated/RootId.js').RootId} RootId
 * @typedef {import('./generated/RootSummary.js').RootSummary} RootSummary
 * @typedef {import('./generated/Settings.js').Settings} Settings
 * @typedef {import('./generated/BoundedSetting.js').BoundedSetting} BoundedSetting
 * @typedef {import('./generated/PreferenceChange.js').PreferenceChange} PreferenceChange
 * @typedef {import('./generated/SpeedLimit.js').SpeedLimit} SpeedLimit
 * @typedef {import('./generated/SpeedLimitSetting.js').SpeedLimitSetting} SpeedLimitSetting
 * @typedef {import('./generated/SpeedLimitChange.js').SpeedLimitChange} SpeedLimitChange
 * @typedef {import('./generated/UpdateSettingsRequest.js').UpdateSettingsRequest} UpdateSettingsRequest
 * @typedef {import('./generated/FolderListing.js').FolderListing} FolderListing
 * @typedef {import('./generated/Action.js').Action} JobAction
 * @typedef {import('./generated/SourceLinkId.js').SourceLinkId} LinkId
 * @typedef {import('./generated/CollectedLink.js').CollectedLink} CollectedLink
 * @typedef {import('./generated/LinkPage.js').LinkPage} LinkPage
 * @typedef {import('./generated/CollectMode.js').CollectMode} CollectMode
 * @typedef {import('./generated/CheckReason.js').CheckReason} CheckReason
 * @typedef {import('./generated/LinkRef.js').LinkRef} LinkRef
 * @typedef {import('./generated/LinksAction.js').LinksAction} LinksAction
 * @typedef {import('./generated/StartRouting.js').StartRouting} StartRouting
 * @typedef {import('./generated/MediaChoice.js').MediaChoice} MediaChoice
 * @typedef {import('./generated/PluginSummary.js').PluginSummary} PluginSummary
 * @typedef {import('./generated/PluginChange.js').PluginChange} PluginChange
 * @typedef {import('./generated/RefusedLink.js').RefusedLink} RefusedLink
 * @typedef {import('./generated/EventKind.js').EventKind} EventKind
 * @typedef {import('./generated/AccountId.js').AccountId} AccountId
 * @typedef {import('./generated/AccountRef.js').AccountRef} AccountRef
 * @typedef {import('./generated/AccountSummary.js').AccountSummary} AccountSummary
 * @typedef {import('./generated/AccountAction.js').AccountAction} AccountAction
 * @typedef {import('./generated/Accounts.js').Accounts} Accounts
 * @typedef {import('./generated/Authorization.js').Authorization} Authorization
 * @typedef {import('./generated/AuthorizationId.js').AuthorizationId} AuthorizationId
 * @typedef {import('./generated/ServiceSupport.js').ServiceSupport} ServiceSupport
 * @typedef {import('./generated/JobRoute.js').JobRoute} JobRoute
 * @typedef {import('./generated/ExtractionSummary.js').ExtractionSummary} ExtractionSummary
 * @typedef {import('./generated/ExtractionState.js').ExtractionState} ExtractionState
 * @typedef {import('./generated/ExtractRequest.js').ExtractRequest} ExtractRequest
 * @typedef {import('./generated/JobCommandRequest.js').JobCommandRequest} JobCommandRequest
 * @typedef {import('./generated/SubmitRequest.js').SubmitRequest} SubmitRequest
 * @typedef {import('./generated/CollectRequest.js').CollectRequest} CollectRequest
 * @typedef {import('./generated/LinksCommandRequest.js').LinksCommandRequest} LinksCommandRequest
 * @typedef {import('./generated/RemoveRootRequest.js').RemoveRootRequest} RemoveRootRequest
 * @typedef {import('./generated/ChangePluginRequest.js').ChangePluginRequest} ChangePluginRequest
 * @typedef {import('./generated/AuthorizeAccountRequest.js').AuthorizeAccountRequest} AuthorizeAccountRequest
 * @typedef {import('./generated/AccountCommandRequest.js').AccountCommandRequest} AccountCommandRequest
 * @typedef {import('./generated/UpdateCheck.js').UpdateCheck} UpdateCheck
 * @typedef {import('./generated/UpdateOutcome.js').UpdateOutcome} UpdateOutcome
 * @typedef {Exclude<JobAction, 'rename' | 'remove' | 'delete_files' | 'cancel_extraction'>} StateAction The
 *   actions that change whether a job runs.
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
 *
 * `permissions` lets views offer only what the engine will allow; the
 * engine still decides. A `local` client (the desktop app) can show the
 * folder dialog and open files by job; a server's client can browse and
 * add server folders instead. No method carries a provider credential.
 * @typedef {{
 *   permissions: readonly Permission[],
 *   local: boolean,
 *   hello: () => Promise<Hello>,
 *   listJobs: (request: {
 *     after?: JobId | null,
 *     pageSize: number,
 *     stages?: readonly JobStage[] | null,
 *   }) => Promise<JobPage>,
 *   inspectJob: (job: JobId) => Promise<JobSnapshot>,
 *   countJobs: () => Promise<JobCounts>,
 *   submit: (request: { idempotencyKey: string, target: string, items: string[] }) => Promise<CommandResult>,
 *   listLinks: (request: { after?: LinkId | null, pageSize: number }) => Promise<LinkPage>,
 *   collectLinks: (request: {
 *     idempotencyKey: string,
 *     target: string,
 *     text: string,
 *     baseUrl?: string | null,
 *     mode?: CollectMode,
 *     destination?: RootId | null,
 *   }) => Promise<CommandResult>,
 *   linksCommand: (request: {
 *     idempotencyKey: string,
 *     target: string,
 *     action: LinksAction,
 *     links: LinkRef[],
 *     destination?: RootId,
 *     account?: AccountId,
 *     routing?: StartRouting,
 *     media?: MediaChoice,
 *   }) => Promise<CommandResult>,
 *   linkSource: (link: LinkId) => Promise<string>,
 *   jobCommand: (job: JobId, request: JobChange) => Promise<CommandResult>,
 *   jobSource: (job: JobId) => Promise<string>,
 *   extract: (job: JobId, request: ExtractChange) => Promise<CommandResult>,
 *   settings: () => Promise<Settings>,
 *   updateSettings: (change: SettingsChange) => Promise<CommandResult>,
 *   removeRoot: (request: {
 *     idempotencyKey: string,
 *     target: string,
 *     expectedRevision: number,
 *     root: RootId,
 *   }) => Promise<CommandResult>,
 *   changePlugin: (request: {
 *     idempotencyKey: string,
 *     target: string,
 *     plugin: string,
 *     change: PluginChange,
 *   }) => Promise<CommandResult>,
 *   accounts: () => Promise<Accounts>,
 *   authorizeAccount: (request: {
 *     idempotencyKey: string,
 *     target: string,
 *     service: string,
 *     account?: AccountRef,
 *     alias?: string,
 *   }) => Promise<CommandResult>,
 *   authorization: (id: AuthorizationId) => Promise<Authorization>,
 *   cancelAuthorization: (id: AuthorizationId) => Promise<Authorization>,
 *   accountCommand: (account: AccountId, request: {
 *     idempotencyKey: string,
 *     target: string,
 *     action: AccountAction,
 *     expectedRevision: number,
 *   }) => Promise<CommandResult>,
 *   refreshAccount: (account: AccountId) => Promise<AccountSummary>,
 *   checkForUpdates: () => Promise<UpdateCheck>,
 *   subscribe: (after: EventCursor | null, sink: StreamSink) => Subscription,
 *   chooseDownloadFolder?: () => Promise<RootSummary | null>,
 *   openFile?: (job: JobId) => Promise<void>,
 *   revealFile?: (job: JobId) => Promise<void>,
 *   browseFolders?: (path: string | null) => Promise<FolderListing>,
 *   addFolder?: (path: string) => Promise<RootSummary>,
 * }} WayfetchClient
 */

/**
 * A settings change at the revision the client saw: each setting given is
 * changed, all or none.
 * @typedef {{
 *   idempotencyKey: string,
 *   target: string,
 *   expectedRevision: number,
 *   defaultRoot?: RootId,
 *   maxActiveDownloads?: PreferenceChange,
 *   downloadSpeedLimit?: SpeedLimitChange,
 *   extractArchives?: boolean,
 * }} SettingsChange
 */

/**
 * A command for one job at the revision the client saw. Stopping unpacking
 * names the unpacking run it stops.
 * @typedef {{
 *   idempotencyKey: string,
 *   target: string,
 *   action: JobAction,
 *   expectedRevision: number,
 *   expectedGeneration?: number,
 *   name?: string,
 *   repeatProviderOperation?: boolean,
 * }} JobChange
 */

/**
 * Unpacking an archive download at the job revision and unpacking run the
 * client saw, with its password if it needs one.
 * @typedef {{
 *   idempotencyKey: string,
 *   target: string,
 *   expectedRevision: number,
 *   expectedGeneration?: number,
 *   password?: string,
 * }} ExtractChange
 */

/**
 * The request both transports send for `change`.
 * @param {JobChange} change
 * @returns {JobCommandRequest}
 */
export function jobCommandRequest({
  idempotencyKey,
  target,
  action,
  expectedRevision,
  expectedGeneration,
  name,
  repeatProviderOperation,
}) {
  /** @type {JobCommandRequest} */
  const request = { idempotency_key: idempotencyKey, target, action, expected_revision: expectedRevision };
  if (expectedGeneration !== undefined) request.expected_generation = expectedGeneration;
  if (name !== undefined) request.name = name;
  if (repeatProviderOperation) request.repeat_provider_operation = true;
  return request;
}

/**
 * The request both transports send for `change`.
 * @param {SettingsChange} change
 * @returns {UpdateSettingsRequest}
 */
export function settingsRequest({
  idempotencyKey,
  target,
  expectedRevision,
  defaultRoot,
  maxActiveDownloads,
  downloadSpeedLimit,
  extractArchives,
}) {
  /** @type {UpdateSettingsRequest} */
  const request = { idempotency_key: idempotencyKey, target, expected_revision: expectedRevision };
  if (defaultRoot !== undefined) request.default_root = defaultRoot;
  if (maxActiveDownloads !== undefined) request.max_active_downloads = maxActiveDownloads;
  if (downloadSpeedLimit !== undefined) request.download_speed_limit = downloadSpeedLimit;
  if (extractArchives !== undefined) request.extract_archives = extractArchives;
  return request;
}

/**
 * The request both transports send for `change`.
 * @param {ExtractChange} change
 * @returns {ExtractRequest}
 */
export function extractRequest({ idempotencyKey, target, expectedRevision, expectedGeneration, password }) {
  /** @type {ExtractRequest} */
  const request = { idempotency_key: idempotencyKey, target, expected_revision: expectedRevision };
  if (expectedGeneration !== undefined) request.expected_generation = expectedGeneration;
  if (password !== undefined) request.password = password;
  return request;
}

// The requests both transports send for each client method.

/**
 * The arguments of the client method `M`.
 * @template {keyof WayfetchClient} M
 * @typedef {Parameters<Extract<WayfetchClient[M], (...args: any[]) => unknown>>} Args
 */

/** @param {Args<'submit'>[0]} request @returns {SubmitRequest} */
export function submitRequest({ idempotencyKey, target, items }) {
  return { idempotency_key: idempotencyKey, target, items };
}

/** @param {Args<'collectLinks'>[0]} request @returns {CollectRequest} */
export function collectRequest({ idempotencyKey, target, text, baseUrl, mode, destination }) {
  /** @type {CollectRequest} */
  const request = { idempotency_key: idempotencyKey, target, text };
  if (baseUrl) request.base_url = baseUrl;
  if (mode) request.mode = mode;
  if (destination) request.destination = destination;
  return request;
}

/** @param {Args<'linksCommand'>[0]} request @returns {LinksCommandRequest} */
export function linksCommandRequest({ idempotencyKey, target, action, links, destination, account, routing, media }) {
  /** @type {LinksCommandRequest} */
  const request = { idempotency_key: idempotencyKey, target, action, links };
  if (destination !== undefined) request.destination = destination;
  if (account !== undefined) request.account = account;
  if (routing !== undefined) request.routing = routing;
  if (media !== undefined) request.media = media;
  return request;
}

/** @param {Args<'removeRoot'>[0]} request @returns {RemoveRootRequest} */
export function removeRootRequest({ idempotencyKey, target, expectedRevision, root }) {
  return { idempotency_key: idempotencyKey, target, expected_revision: expectedRevision, root };
}

/** @param {Args<'changePlugin'>[0]} request @returns {ChangePluginRequest} */
export function changePluginRequest({ idempotencyKey, target, plugin, change }) {
  return { idempotency_key: idempotencyKey, target, plugin, change };
}

/** @param {Args<'authorizeAccount'>[0]} request @returns {AuthorizeAccountRequest} */
export function authorizeAccountRequest({ idempotencyKey, target, service, account, alias }) {
  /** @type {AuthorizeAccountRequest} */
  const request = { idempotency_key: idempotencyKey, target, service };
  if (account) request.account = account;
  if (alias) request.alias = alias;
  return request;
}

/** @param {Args<'accountCommand'>[1]} request @returns {AccountCommandRequest} */
export function accountCommandRequest({ idempotencyKey, target, action, expectedRevision }) {
  return { idempotency_key: idempotencyKey, target, action, expected_revision: expectedRevision };
}

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
   * @param {{ currentRevision?: number, retryAfter?: number, retryAt?: number }} [details]
   */
  constructor(code, message, details = {}) {
    super(message);
    this.name = 'WayfetchError';
    this.code = code;
    /** The record's revision, for `revision_conflict`. */
    this.currentRevision = details.currentRevision;
    /** Seconds to wait before trying again, when the server said so. */
    this.retryAfter = details.retryAfter;
    /**
     * When a refused retry may be asked for again (milliseconds since the
     * epoch), for a failure with a deadline still ahead.
     */
    this.retryAt = details.retryAt;
  }
}

/**
 * Whether an engine speaking `engine` serves this UI: the same major
 * version, and nothing newer than it provides; before 1.0, the same version
 * (`ProtocolVersion::serves`).
 * @param {ProtocolVersion} engine
 */
export function serves(engine) {
  if (engine.major === 0) return PROTOCOL.major === 0 && engine.minor === PROTOCOL.minor;
  return engine.major === PROTOCOL.major && PROTOCOL.minor <= engine.minor;
}

/** A fresh idempotency key. */
export function idempotencyKey() {
  return crypto.randomUUID();
}
