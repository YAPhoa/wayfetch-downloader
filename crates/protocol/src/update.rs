//! Whether a newer release of the application is published, checked only
//! when the person asks ([`crate::Query::Updates`]).

use serde::{Deserialize, Serialize};
use wayfetch_domain::name::DiagnosticText;

/// The answer to an update check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct UpdateCheck {
    /// The running version, such as `0.1.0`.
    pub current: String,
    /// Where every release is listed.
    pub releases: String,
    /// What the check found.
    pub outcome: UpdateOutcome,
}

/// What an update check found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum UpdateOutcome {
    /// A newer release is published.
    Available {
        /// Its version, such as `0.2.0`.
        version: String,
        /// Its release page.
        url: String,
    },
    /// No published release is newer than the running version.
    UpToDate,
    /// The releases could not be read; `problem` says why.
    Unavailable {
        /// Why, without any address or response body.
        problem: DiagnosticText,
    },
}
