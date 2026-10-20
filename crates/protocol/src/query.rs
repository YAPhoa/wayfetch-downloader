//! Queries: bounded, paginated reads (architecture brief sections 4.6 and
//! 11.1, test T48).
//!
//! List results use keyset pagination in ascending job ID order: a page
//! carries the last ID it returned, and the next request continues after it.
//! Each page also carries the event cursor the snapshot is consistent with,
//! so a client can subscribe from exactly that point without missing or
//! double-applying changes.

use std::collections::BTreeMap;
use std::num::NonZeroU16;

use serde::{Deserialize, Serialize};
use wayfetch_domain::failure::FailureCode;
use wayfetch_domain::id::{AccountId, BatchId, JobId, PackageId, RootId, SourceLinkId};
use wayfetch_domain::job::{DesiredState, DisplayState, JobStage};
use wayfetch_domain::name::{DiagnosticText, HostName, ProviderId};
use wayfetch_domain::time::UtcTimestamp;
use wayfetch_domain::version::{Generation, Revision};

use crate::account::{Accounts, Authorization, AuthorizationId};
use crate::command::Permission;
use crate::error::{ErrorCode, ProtocolError};
use crate::event::EventCursor;
use crate::media::{JobMedia, LinkMedia, PluginSummary};

/// Items per page: 1 to [`PageSize::MAX`], default [`PageSize::DEFAULT`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u16", into = "u16")]
pub struct PageSize(NonZeroU16);

impl PageSize {
    /// Largest page.
    pub const MAX: NonZeroU16 = NonZeroU16::new(500).unwrap();
    /// Page size when the client does not choose.
    pub const DEFAULT: Self = Self(NonZeroU16::new(100).unwrap());

    /// The size.
    pub const fn get(self) -> u16 {
        self.0.get()
    }
}

impl Default for PageSize {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl TryFrom<u16> for PageSize {
    type Error = ProtocolError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match NonZeroU16::new(value) {
            Some(n) if n <= Self::MAX => Ok(Self(n)),
            Some(_) => Err(ProtocolError::new(
                ErrorCode::LimitExceeded,
                "page size too large",
            )),
            None => Err(ProtocolError::new(
                ErrorCode::InvalidRequest,
                "page size must be positive",
            )),
        }
    }
}

impl From<PageSize> for u16 {
    fn from(value: PageSize) -> Self {
        value.get()
    }
}

/// Which jobs to list. Empty fields match everything.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct JobFilter {
    /// Only jobs in one of these stages. At most one entry per stage.
    #[serde(default, deserialize_with = "bounded_stages")]
    #[cfg_attr(feature = "ts", ts(as = "Option<Vec<JobStage>>", optional))]
    pub stages: Vec<JobStage>,
    /// Only jobs in this package.
    #[serde(default)]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub package: Option<PackageId>,
}

fn bounded_stages<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<JobStage>, D::Error> {
    use strum::VariantArray;
    let stages = Vec::<JobStage>::deserialize(d)?;
    if stages.len() > JobStage::VARIANTS.len() {
        return Err(serde::de::Error::custom("too many stage filters"));
    }
    Ok(stages)
}

/// A read request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum Query {
    /// One job's current state.
    InspectJob {
        /// The job.
        job: JobId,
    },
    /// A page of jobs in ascending ID order.
    ListJobs {
        /// Which jobs.
        #[serde(default)]
        #[cfg_attr(feature = "ts", ts(as = "Option<JobFilter>", optional))]
        filter: JobFilter,
        /// Continue after this job ID (from the previous page's `next_after`).
        #[serde(default)]
        #[cfg_attr(feature = "ts", ts(optional))]
        after: Option<JobId>,
        /// Items per page.
        #[serde(default)]
        #[cfg_attr(feature = "ts", ts(as = "Option<u16>", optional))]
        page_size: PageSize,
    },
    /// How many jobs are in each stage.
    CountJobs {
        /// Which jobs.
        #[serde(default)]
        #[cfg_attr(feature = "ts", ts(as = "Option<JobFilter>", optional))]
        filter: JobFilter,
    },
    /// The user's settings and the destinations they may choose.
    Settings {},
    /// The link a job was submitted with. Needs
    /// [`Permission::Sources`], since a link can carry a signed token.
    JobSource {
        /// The job.
        job: JobId,
    },
    /// A page of collected links in ascending ID order, which keeps each
    /// batch's links together.
    ListLinks {
        /// Continue after this link (from the previous page's
        /// `next_after`).
        #[serde(default)]
        #[cfg_attr(feature = "ts", ts(optional))]
        after: Option<SourceLinkId>,
        /// Items per page.
        #[serde(default)]
        #[cfg_attr(feature = "ts", ts(as = "Option<u16>", optional))]
        page_size: PageSize,
    },
    /// A collected link exactly as it was entered. Needs
    /// [`Permission::Sources`].
    LinkSource {
        /// The link.
        link: SourceLinkId,
    },
    /// The accounts the user added, redacted, and the services accounts
    /// can be added for. Needs [`Permission::Accounts`]. Never contacts a
    /// provider.
    Accounts {},
    /// Where an authorization started by
    /// [`crate::Command::AuthorizeAccount`] is. Needs
    /// [`Permission::Accounts`]. Never contacts a provider.
    Authorization {
        /// The authorization.
        id: AuthorizationId,
    },
}

impl Query {
    /// The permission the caller needs.
    pub const fn required_permission(&self) -> Permission {
        match self {
            Self::JobSource { .. } | Self::LinkSource { .. } => Permission::Sources,
            Self::Accounts {} | Self::Authorization { .. } => Permission::Accounts,
            Self::InspectJob { .. }
            | Self::ListJobs { .. }
            | Self::CountJobs { .. }
            | Self::Settings {}
            | Self::ListLinks { .. } => Permission::Read,
        }
    }
}

/// The most recent failure of a job, as shown to clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct FailureSummary {
    /// Stable code.
    pub code: FailureCode,
    /// Earliest automatic retry, if one is scheduled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_at: Option<UtcTimestamp>,
    /// Redacted context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<DiagnosticText>,
}

/// A job's client-visible state. Durable fields only; live transfer rate is
/// in progress samples.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct JobSummary {
    /// The job.
    pub id: JobId,
    /// Its package.
    pub package: PackageId,
    /// A safe file-name hint from the source path or accepted provider/response
    /// metadata. Absent for opaque links without a meaningful file name. The
    /// published artifact's path takes precedence. Boxed to keep event
    /// summaries small.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<Box<str>>,
    /// Observed stage.
    pub stage: JobStage,
    /// Requested state.
    pub desired: DesiredState,
    /// What to show, including pausing and stopping.
    pub display: DisplayState,
    /// Revision to present with the next command for this job.
    pub revision: Revision,
    /// Current execution generation.
    pub generation: Generation,
    /// Bytes durably checkpointed.
    pub bytes_done: u64,
    /// Expected total, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_total: Option<u64>,
    /// Most recent failure, if the job is waiting, blocked, or failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<FailureSummary>,
    /// The published file, once the job is complete.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<ArtifactSummary>,
    /// Host of the submitted link, which may differ from the provider that
    /// serves it. Never the whole link, which can carry a signed token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_host: Option<HostName>,
    /// The root the job saves its file in, fixed when it was submitted or,
    /// for a job submitted before it could be, when it first started.
    /// Absent until then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<RootId>,
    /// Why the job's latest attempt discarded its saved data and started
    /// from zero, such as a file that changed on the server. Kept until the
    /// job is retried again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_reason: Option<DiagnosticText>,
    /// The provider account the job downloads through, chosen when it was
    /// started; absent for a direct download. Names the account only by
    /// its ID: its alias and username are for clients that may see
    /// accounts. Boxed, as most jobs have none, to keep summaries and the
    /// events carrying them small.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub route: Option<Box<JobRoute>>,
    /// For a download of a media link: its service and what it is made
    /// as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub media: Option<Box<JobMedia>>,
    /// For a complete download of an archive, or of the first part of a
    /// multi-part archive: whether and where it was unpacked. Boxed, as
    /// most jobs have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub extraction: Option<Box<ExtractionSummary>>,
}

/// Where unpacking a downloaded archive stands.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::IntoStaticStr,
    strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum ExtractionState {
    /// Not unpacked, and not going to be unless asked.
    Idle,
    /// Waiting for other parts of the archive to finish downloading.
    Waiting,
    /// Waiting for its turn.
    Queued,
    /// Being unpacked.
    Extracting,
    /// The archive is protected: it needs its password.
    NeedsPassword,
    /// Unpacked; `output` names what was made.
    Complete,
    /// Unpacking failed; `problem` says why. The archive is kept.
    Failed,
    /// Unpacking was cancelled. The archive is kept.
    Cancelled,
}

/// Unpacking a downloaded archive into a new folder next to it. The
/// archive itself is never changed or removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ExtractionSummary {
    /// Where it stands.
    pub state: ExtractionState,
    /// The unpacking run, absent until the archive is first queued. Each
    /// request to unpack starts a new one; stopping or unpacking again
    /// names the run the client saw.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub generation: Option<Generation>,
    /// Once complete, what was made, in the archive's root: a new folder,
    /// or the archive's only entry if it held just one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub output: Option<String>,
    /// Why it failed, is waiting, or needs a password again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub problem: Option<DiagnosticText>,
}

/// How a job is downloaded, when it is not directly from its host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct JobRoute {
    /// The provider's service, as in the accounts' services.
    pub service: ProviderId,
    /// The account. It may have been removed since; the job then fails
    /// rather than trying another route.
    pub account: AccountId,
}

/// A file a job published.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ArtifactSummary {
    /// The destination root holding it.
    pub root: RootId,
    /// Its path inside the root, `/`-separated.
    pub path: String,
    /// Its size in bytes.
    pub size: u64,
}

/// One page of a list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Page<T> {
    /// The items, at most the requested page size.
    pub items: Vec<T>,
    /// Pass as `after` to get the next page; absent on the last page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_after: Option<JobId>,
    /// How many items that match the query come before this page, so a
    /// client can say where the page sits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub before: Option<u64>,
    /// The page reflects every durable event up to and including this
    /// cursor. Subscribe after it to follow later changes.
    pub as_of: EventCursor,
}

/// The number of jobs in each stage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct JobCounts {
    /// Jobs per stage; stages with no jobs are absent.
    pub stages: BTreeMap<JobStage, u64>,
    /// The counts reflect every durable event up to and including this
    /// cursor.
    pub as_of: EventCursor,
}

/// A destination root a client may choose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct RootSummary {
    /// The root.
    pub id: RootId,
    /// Where it is, on the engine's host: a server path for a remote
    /// client.
    pub path: String,
    /// Whether it could be opened and verified just now.
    pub available: bool,
    /// Why it is unavailable, if it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<DiagnosticText>,
    /// Whether the user added it, rather than the operator's configuration.
    /// [`crate::Command::RemoveRoot`] removes any folder but the default.
    #[serde(default)]
    pub added: bool,
}

/// The most folders a [`FolderListing`] names.
pub const MAX_LISTED_FOLDERS: usize = 1000;

/// The folders directly inside a folder on the engine's host, for choosing
/// one to add as a destination. Hidden folders (named with a leading dot)
/// and names that are not valid UTF-8 are left out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct FolderListing {
    /// The folder, as an absolute path with links and `..` resolved.
    pub path: String,
    /// The folder containing it; absent at the top of the filesystem.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub parent: Option<String>,
    /// The names of the folders inside it, in a case-insensitive order: at
    /// most [`MAX_LISTED_FOLDERS`].
    pub folders: Vec<String>,
    /// Whether more folders were left out than the listing holds.
    pub truncated: bool,
}

/// The user's settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Settings {
    /// Revision to present with the next settings change.
    pub revision: Revision,
    /// Where newly submitted jobs are saved: the chosen default, or the
    /// operator's download root until one is chosen. Absent if there is
    /// neither, and new jobs then wait.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_root: Option<RootId>,
    /// The destinations a client may choose, in ID order.
    pub roots: Vec<RootSummary>,
    /// Most downloads transferring at once, engine-wide.
    pub max_active_downloads: BoundedSetting,
    /// The combined rate of every download's body, engine-wide.
    pub download_speed_limit: SpeedLimitSetting,
    /// The helper programs media services run, and whether each can be
    /// used.
    #[serde(default)]
    pub plugins: Vec<PluginSummary>,
    /// Whether downloaded archives are unpacked as they finish.
    pub extract_archives: bool,
}

/// An engine preference with a whole-number value, as the operator's
/// policy and the user's saved choice resolve it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct BoundedSetting {
    /// The value in effect.
    pub value: u64,
    /// Where `value` comes from.
    pub source: SettingSource,
    /// The value without a saved preference: the operator's configured
    /// default, else the built-in one. Choosing "use the default" applies
    /// it.
    pub default: u64,
    /// The user's saved preference, if any. It is kept when the policy
    /// changes, even if the policy no longer allows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub saved: Option<u64>,
    /// Smallest value the policy permits.
    pub min: u64,
    /// Largest value the policy permits.
    pub max: u64,
    /// Whether the operator fixed the value: it cannot be changed here,
    /// and `value` is the configured one.
    pub locked: bool,
    /// Whether the saved preference is outside the policy, so `value` is
    /// the nearest permitted value instead (the configured one when
    /// locked).
    pub conflict: bool,
}

/// Where an engine preference's value comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum SettingSource {
    /// Wayfetch's own default.
    BuiltIn,
    /// The server's configuration: a configured default, or a fixed value.
    Server,
    /// The user's saved preference.
    Saved,
}

/// A cap on the combined rate of download bodies, in bytes per second.
///
/// On the wire, `"unlimited"` or `{"bytes_per_second": n}`. No number
/// stands for Unlimited: zero is refused like any value below the
/// permitted range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum SpeedLimit {
    /// No cap.
    Unlimited,
    /// At most this many bytes of download bodies per second, across every
    /// download together.
    BytesPerSecond(u64),
}

/// The total download speed limit, as the operator's policy and the
/// user's saved choice resolve it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct SpeedLimitSetting {
    /// The limit in effect.
    pub value: SpeedLimit,
    /// Where `value` comes from.
    pub source: SettingSource,
    /// The limit without a saved preference: the operator's configured
    /// default, else Unlimited. Choosing "use the default" applies it.
    pub default: SpeedLimit,
    /// The user's saved preference, if any. It is kept when the policy
    /// changes, even if the policy no longer allows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub saved: Option<SpeedLimit>,
    /// Smallest finite limit the policy permits, in bytes per second.
    pub min: u64,
    /// Largest finite limit the policy permits, in bytes per second.
    pub max: u64,
    /// Whether Unlimited is permitted. An operator's maximum forbids it.
    pub unlimited: bool,
    /// Whether the operator fixed the limit: it cannot be changed here,
    /// and `value` is the configured one.
    pub locked: bool,
    /// Whether the saved preference is outside the policy, so `value` is
    /// the nearest permitted limit instead (the configured one when
    /// locked; the maximum for a saved Unlimited the policy forbids).
    pub conflict: bool,
}

/// What is known about a collected link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum LinkCheck {
    /// A valid `http` or `https` link that was not checked: whether it
    /// serves a file is not known. It can be analyzed.
    NotChecked,
    /// Its check is waiting or running.
    Checking,
    /// It served a file, or a video service read it, when last checked.
    /// Not a promise that a download will succeed.
    Online,
    /// Its server said it is gone; `reason` says how.
    Offline,
    /// Checking could not tell; `reason` and `check_detail` say why. It
    /// can still be started.
    Unknown,
    /// It is a web page, not a file. Once analyzed, the files found on it
    /// are listed beside it.
    Page,
    /// Not a link Wayfetch can download; `problem` says why.
    Invalid,
}

/// Why a check found a link offline or could not tell.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum CheckReason {
    /// The server says there is nothing there (404 or 410).
    NotFound,
    /// The server wants a sign-in, or refuses access (401, 403, 407).
    AccessDenied,
    /// The server asks to slow down (429).
    RateLimited,
    /// No answer in time.
    TimedOut,
    /// The server could not be reached.
    Unreachable,
    /// The network policy forbids the address, such as a private one.
    Blocked,
    /// The server failed (5xx).
    ServerError,
    /// The answer could not be understood.
    Unreadable,
    /// The page is larger than Wayfetch reads.
    TooLarge,
}

/// A link in the collected list: waiting to be started, or started and
/// kept until removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct CollectedLink {
    /// The link.
    pub id: SourceLinkId,
    /// The batch it was collected with.
    pub batch: BatchId,
    /// When that batch was collected.
    pub collected_at: UtcTimestamp,
    /// Revision to present with the next command for this link.
    pub revision: Revision,
    /// What is known about it.
    pub check: LinkCheck,
    /// Its host, for a valid link. Never the whole link, which can carry
    /// a signed token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub host: Option<HostName>,
    /// A file name taken from the link's path: only a hint, since the
    /// server names the file when it is downloaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub name_hint: Option<String>,
    /// The entry as typed, for an invalid link, with any credentials,
    /// query, and fragment left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub text: Option<String>,
    /// Where it will be saved; absent for the operator's download folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub destination: Option<RootId>,
    /// The destination's path, kept after the folder stops being
    /// permitted: a link saving to a removed folder still says which, and
    /// is refused at start until given another.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub destination_path: Option<String>,
    /// The download it was added as, once started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub job: Option<JobId>,
    /// Why it is invalid, or why it was last refused when started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub problem: Option<DiagnosticText>,
    /// For an offline link or one whose check could not tell: why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub reason: Option<CheckReason>,
    /// What its last check said beyond `reason`, such as the server's
    /// status, or that links on a page were left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub check_detail: Option<DiagnosticText>,
    /// The file's size, when its server said. Absent when unknown, which
    /// is not zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub size: Option<u64>,
    /// The page it was found on, without the page link's query or
    /// fragment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub found_on: Option<String>,
    /// For a page that was analyzed: links on it collected beside it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub found: Option<u32>,
    /// For a page that was analyzed: links on it that lead to other pages,
    /// which are never followed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub skipped: Option<u32>,
    /// The service whose provider answered the last check, for a link an
    /// enabled account would download; absent when the link's own server
    /// did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub checked_via: Option<ProviderId>,
    /// For a link a media service recognized, such as a YouTube video:
    /// its title, once checked, and what it will be downloaded as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub media: Option<Box<LinkMedia>>,
}

/// One page of collected links.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct LinkPage {
    /// The links, at most the requested page size.
    pub items: Vec<CollectedLink>,
    /// Pass as `after` to get the next page; absent on the last page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub next_after: Option<SourceLinkId>,
    /// How many links are collected in all.
    pub total: u64,
    /// How many collected links come before this page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub before: Option<u64>,
    /// How many of them are waiting for, or in, a check.
    #[serde(default)]
    pub checking: u64,
    /// The page reflects every durable event up to and including this
    /// cursor.
    pub as_of: EventCursor,
}

/// A query's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum QueryResult {
    /// Answer to [`Query::InspectJob`].
    Job {
        /// The job.
        job: JobSummary,
        /// Snapshot cursor, as in [`Page::as_of`].
        as_of: EventCursor,
    },
    /// Answer to [`Query::ListJobs`].
    Jobs(Page<JobSummary>),
    /// Answer to [`Query::CountJobs`].
    JobCounts(JobCounts),
    /// Answer to [`Query::Settings`].
    Settings(Settings),
    /// Answer to [`Query::JobSource`].
    JobSource {
        /// The job.
        job: JobId,
        /// The link exactly as it was submitted.
        url: String,
    },
    /// Answer to [`Query::ListLinks`].
    Links(LinkPage),
    /// Answer to [`Query::LinkSource`].
    LinkSource {
        /// The link.
        link: SourceLinkId,
        /// The entry exactly as it was collected; for an invalid one, less
        /// any credentials it held.
        url: String,
    },
    /// Answer to [`Query::Accounts`].
    Accounts(Accounts),
    /// Answer to [`Query::Authorization`].
    Authorization(Authorization),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{from_value, json};

    #[test]
    fn page_size_bounds() {
        assert_eq!(PageSize::try_from(1).unwrap().get(), 1);
        assert_eq!(PageSize::try_from(500).unwrap().get(), 500);
        assert_eq!(
            PageSize::try_from(0).unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            PageSize::try_from(501).unwrap_err().code,
            ErrorCode::LimitExceeded
        );
    }

    #[test]
    fn list_defaults() {
        let q: Query = from_value(json!({"type": "list_jobs"})).unwrap();
        assert_eq!(
            q,
            Query::ListJobs {
                filter: JobFilter::default(),
                after: None,
                page_size: PageSize::DEFAULT
            }
        );
        let q: Query = from_value(json!({
            "type": "list_jobs",
            "filter": {"stages": ["queued", "paused"]},
            "after": "job-9",
            "page_size": 50
        }))
        .unwrap();
        let Query::ListJobs {
            filter,
            after,
            page_size,
        } = q
        else {
            panic!()
        };
        assert_eq!(filter.stages, [JobStage::Queued, JobStage::Paused]);
        assert_eq!(after, JobId::from_u64(9));
        assert_eq!(page_size.get(), 50);
    }

    #[test]
    fn filters_are_bounded_and_strict() {
        let many = vec!["queued"; 14];
        assert!(
            from_value::<Query>(json!({"type": "list_jobs", "filter": {"stages": many}})).is_err()
        );
        assert!(from_value::<Query>(json!({"type": "list_jobs", "page_size": 10000})).is_err());
        assert!(
            from_value::<Query>(json!({"type": "list_jobs", "filter": {"sql": "1=1"}})).is_err()
        );
        assert!(
            from_value::<Query>(json!({"type": "count_jobs", "filter": {"stages": many}})).is_err()
        );
        assert!(from_value::<Query>(json!({"type": "count_jobs", "after": "job-1"})).is_err());
    }

    #[test]
    fn counts_wire_form() {
        let q: Query = from_value(json!({"type": "count_jobs"})).unwrap();
        assert_eq!(
            q,
            Query::CountJobs {
                filter: JobFilter::default()
            }
        );
        let instance = crate::InstanceId::generate();
        let counts = QueryResult::JobCounts(JobCounts {
            stages: BTreeMap::from([(JobStage::Transferring, 2), (JobStage::Queued, 5)]),
            as_of: EventCursor {
                instance,
                position: 9,
            },
        });
        assert_eq!(
            serde_json::to_value(&counts).unwrap(),
            json!({
                "type": "job_counts",
                "stages": {"queued": 5, "transferring": 2},
                "as_of": {"instance": instance, "position": 9},
            })
        );
    }

    #[test]
    fn summaries_recorded_before_the_host_still_decode() {
        let old = json!({
            "id": "job-1", "package": "pkg-1", "stage": "queued", "desired": "running",
            "display": "queued", "revision": 1, "generation": 1, "bytes_done": 0,
        });
        let job: JobSummary = from_value(old.clone()).unwrap();
        assert_eq!(job.original_host, None);
        assert_eq!(job.destination, None);
        assert_eq!(job.name, None);
        let mut new = old;
        new["original_host"] = json!("files.example");
        new["destination"] = json!("root-2");
        new["name"] = json!("My File.zip");
        let job: JobSummary = from_value(new).unwrap();
        assert_eq!(job.original_host.unwrap().as_str(), "files.example");
        assert_eq!(job.destination, RootId::from_u64(2));
        assert_eq!(job.name.as_deref(), Some("My File.zip"));
    }

    #[test]
    fn sources_need_their_own_permission() {
        let q: Query = from_value(json!({"type": "job_source", "job": "job-2"})).unwrap();
        assert_eq!(q.required_permission(), Permission::Sources);
        assert_eq!(Query::Settings {}.required_permission(), Permission::Read);
    }

    #[test]
    fn accounts_need_their_own_permission() {
        let q: Query = from_value(json!({"type": "accounts"})).unwrap();
        assert_eq!(q.required_permission(), Permission::Accounts);
        let q: Query = from_value(json!({
            "type": "authorization",
            "id": "6f1c2c5e-6c1e-4b8a-9f57-3a8f9f0f6a11",
        }))
        .unwrap();
        assert_eq!(q.required_permission(), Permission::Accounts);
    }

    #[test]
    fn settings_wire_form() {
        assert_eq!(
            from_value::<Query>(json!({"type": "settings"})).unwrap(),
            Query::Settings {}
        );
        assert!(from_value::<Query>(json!({"type": "settings", "roots": []})).is_err());
        let settings = QueryResult::Settings(Settings {
            revision: Revision::from_u64(3).unwrap(),
            default_root: RootId::from_u64(2),
            roots: vec![RootSummary {
                id: RootId::from_u64(2).unwrap(),
                path: "/media".into(),
                available: false,
                problem: Some(DiagnosticText::new("missing")),
                added: true,
            }],
            max_active_downloads: BoundedSetting {
                value: 8,
                source: SettingSource::Saved,
                default: 4,
                saved: Some(8),
                min: 1,
                max: 16,
                locked: false,
                conflict: false,
            },
            download_speed_limit: SpeedLimitSetting {
                value: SpeedLimit::BytesPerSecond(2_097_152),
                source: SettingSource::Server,
                default: SpeedLimit::BytesPerSecond(2_097_152),
                saved: Some(SpeedLimit::Unlimited),
                min: 1024,
                max: 4_194_304,
                unlimited: false,
                locked: false,
                conflict: true,
            },
            plugins: vec![PluginSummary {
                id: "yt-dlp".into(),
                name: "yt-dlp".into(),
                role: crate::media::PluginRole::MediaResolver,
                services: vec![ProviderId::new("youtube").unwrap()],
                state: crate::media::PluginState::Ready,
                version: Some("2026.08.19".into()),
                problem: None,
                enabled: true,
                updatable: true,
                installed: true,
                fallback_version: Some("2026.07.04".into()),
            }],
            extract_archives: true,
        });
        assert_eq!(
            serde_json::to_value(&settings).unwrap(),
            json!({
                "type": "settings",
                "revision": 3,
                "default_root": "root-2",
                "roots": [{"id": "root-2", "path": "/media", "available": false, "problem": "missing", "added": true}],
                "max_active_downloads": {
                    "value": 8, "source": "saved", "default": 4, "saved": 8,
                    "min": 1, "max": 16, "locked": false, "conflict": false,
                },
                "download_speed_limit": {
                    "value": {"bytes_per_second": 2_097_152}, "source": "server",
                    "default": {"bytes_per_second": 2_097_152}, "saved": "unlimited",
                    "min": 1024, "max": 4_194_304, "unlimited": false,
                    "locked": false, "conflict": true,
                },
                "plugins": [{
                    "id": "yt-dlp", "name": "yt-dlp", "role": "media_resolver",
                    "services": ["youtube"], "state": "ready", "version": "2026.08.19",
                    "enabled": true, "updatable": true, "installed": true,
                    "fallback_version": "2026.07.04",
                }],
                "extract_archives": true,
            })
        );
        // A 4.0 engine's plugins carry none of 4.1's fields: each is on,
        // and none can be updated or removed.
        let older: crate::media::PluginSummary = from_value(json!({
            "id": "ffmpeg", "name": "ffmpeg", "role": "media_converter",
            "services": [], "state": "missing",
        }))
        .unwrap();
        assert!(older.enabled && !older.updatable && !older.installed);
        // A 1.0 engine's roots carry no `added`: none can be removed.
        let older: RootSummary =
            from_value(json!({"id": "root-2", "path": "/media", "available": true})).unwrap();
        assert!(!older.added);
    }
}
