//! Queries: bounded, paginated reads (architecture brief sections 4.6 and
//! 11.1, test T48).
//!
//! List results use keyset pagination in ascending job ID order: a page
//! carries the last ID it returned, and the next request continues after it.
//! Each page also carries the event cursor the snapshot is consistent with,
//! so a client can subscribe from exactly that point without missing or
//! double-applying changes.

use std::num::NonZeroU16;

use serde::{Deserialize, Serialize};
use wayfetch_domain::failure::FailureCode;
use wayfetch_domain::id::{JobId, PackageId, RootId};
use wayfetch_domain::job::{DesiredState, DisplayState, JobStage};
use wayfetch_domain::name::DiagnosticText;
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
pub struct JobFilter {
    /// Only jobs in one of these stages. At most one entry per stage.
    #[serde(default, deserialize_with = "bounded_stages")]
    pub stages: Vec<JobStage>,
    /// Only jobs in this package.
    #[serde(default)]
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
        filter: JobFilter,
        /// Continue after this job ID (from the previous page's `next_after`).
        #[serde(default)]
        after: Option<JobId>,
        /// Items per page.
        #[serde(default)]
        page_size: PageSize,
    },
}

impl Query {
    /// The permission the caller needs.
    pub const fn required_permission(&self) -> Permission {
        Permission::Read
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
    }
}
