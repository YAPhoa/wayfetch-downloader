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
use wayfetch_domain::id::{JobId, PackageId, RootId};
use wayfetch_domain::job::{DesiredState, DisplayState, JobStage};
use wayfetch_domain::name::{DiagnosticText, HostName};
use wayfetch_domain::time::UtcTimestamp;
use wayfetch_domain::version::{Generation, Revision};

use crate::command::Permission;
use crate::error::{ErrorCode, ProtocolError};
use crate::event::EventCursor;

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
    /// How many jobs are in each stage. Added in protocol 0.2.
    CountJobs {
        /// Which jobs.
        #[serde(default)]
        #[cfg_attr(feature = "ts", ts(as = "Option<JobFilter>", optional))]
        filter: JobFilter,
    },
    /// The user's settings and the destinations they may choose. Added in
    /// protocol 0.3.
    Settings {},
    /// The link a job was submitted with. Needs
    /// [`Permission::Sources`], since a link can carry a signed token.
    /// Added in protocol 1.0.
    JobSource {
        /// The job.
        job: JobId,
    },
}

impl Query {
    /// The permission the caller needs.
    pub const fn required_permission(&self) -> Permission {
        match self {
            Self::JobSource { .. } => Permission::Sources,
            Self::InspectJob { .. }
            | Self::ListJobs { .. }
            | Self::CountJobs { .. }
            | Self::Settings {} => Permission::Read,
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
    /// Added in protocol 0.2; absent from events recorded before it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_host: Option<HostName>,
    /// The root the job saves its file in, fixed when it was submitted or,
    /// for a job submitted before it could be, when it first started.
    /// Absent until then. Added in protocol 0.3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<RootId>,
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
    /// Whether the user added it, rather than the operator's configuration,
    /// so [`crate::Command::RemoveRoot`] may remove it. Added in protocol
    /// 1.1.
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

/// The user's settings. Added in protocol 0.3.
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
        let mut new = old;
        new["original_host"] = json!("files.example");
        new["destination"] = json!("root-2");
        let job: JobSummary = from_value(new).unwrap();
        assert_eq!(job.original_host.unwrap().as_str(), "files.example");
        assert_eq!(job.destination, RootId::from_u64(2));
    }

    #[test]
    fn sources_need_their_own_permission() {
        let q: Query = from_value(json!({"type": "job_source", "job": "job-2"})).unwrap();
        assert_eq!(q.required_permission(), Permission::Sources);
        assert_eq!(Query::Settings {}.required_permission(), Permission::Read);
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
        });
        assert_eq!(
            serde_json::to_value(&settings).unwrap(),
            json!({
                "type": "settings",
                "revision": 3,
                "default_root": "root-2",
                "roots": [{"id": "root-2", "path": "/media", "available": false, "problem": "missing", "added": true}],
            })
        );
        // A 1.0 engine's roots carry no `added`: none can be removed.
        let older: RootSummary =
            from_value(json!({"id": "root-2", "path": "/media", "available": true})).unwrap();
        assert!(!older.added);
    }
}
