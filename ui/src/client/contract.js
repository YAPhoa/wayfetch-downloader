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
export const PROTOCOL = Object.freeze({ major: 3, minor: 0 });

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
 * @typedef {import('./generated/LinkRef.js').LinkRef} LinkRef
 * @typedef {import('./generated/LinksAction.js').LinksAction} LinksAction
 * @typedef {import('./generated/StartRouting.js').StartRouting} StartRouting
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
 * @typedef {Exclude<JobAction, 'rename' | 'remove' | 'delete_files'>} StateAction The
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
 * `permissions` is what this client may do, so views offer only what the
 * engine will allow; the engine still decides. `local` says whether the
 * engine's paths are on the computer showing the page (the desktop app)
 * or on a server. Only a local host can open files and show the folder
 * dialog, so only its client has `chooseDownloadFolder`, `openFile`, and
 * `revealFile`; each takes a job or nothing, never a path. A server's
 * client instead has `browseFolders` and `addFolder`, which list the
 * server's folders and add one the owner chose as a download folder.
 *
 * The account methods need the `accounts` permission. None takes or
 * returns a provider credential: signing in happens on the provider's
 * site, and the engine keeps what the provider issues.
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
 *     items: string[],
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
 *   }) => Promise<CommandResult>,
 *   linkSource: (link: LinkId) => Promise<string>,
 *   jobCommand: (job: JobId, request: {
 *     idempotencyKey: string,
 *     target: string,
 *     action: JobAction,
 *     expectedRevision: number,
 *     name?: string,
 *     repeatProviderOperation?: boolean,
 *   }) => Promise<CommandResult>,
 *   jobSource: (job: JobId) => Promise<string>,
 *   settings: () => Promise<Settings>,
 *   updateSettings: (change: SettingsChange) => Promise<CommandResult>,
 *   removeRoot: (request: {
 *     idempotencyKey: string,
 *     target: string,
 *     expectedRevision: number,
 *     root: RootId,
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
 * }} SettingsChange
 */

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
}) {
  /** @type {UpdateSettingsRequest} */
  const request = { idempotency_key: idempotencyKey, target, expected_revision: expectedRevision };
  if (defaultRoot !== undefined) request.default_root = defaultRoot;
  if (maxActiveDownloads !== undefined) request.max_active_downloads = maxActiveDownloads;
  if (downloadSpeedLimit !== undefined) request.download_speed_limit = downloadSpeedLimit;
  return request;
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
