//! Whether a newer release of the application is published
//! ([`crate::Query::Updates`]), and, in a host that installs updates
//! itself, how far it has got with one.

use serde::{Deserialize, Serialize};
use wayfetch_domain::name::DiagnosticText;
use wayfetch_domain::time::UtcTimestamp;

/// What is known about updates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct UpdateCheck {
    /// The running version, such as `0.1.0`.
    pub current: String,
    /// Where every release is listed.
    pub releases: String,
    /// What the last check found.
    pub outcome: UpdateOutcome,
    /// When that check ended; absent before the first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub checked_at: Option<UtcTimestamp>,
    /// The newer release's installation, in a host that installs updates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub install: Option<UpdateInstall>,
}

/// What an update check found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum UpdateOutcome {
    /// Nothing has been checked yet.
    NotChecked,
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

/// How far a host that installs updates has got with the newer release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum UpdateInstall {
    /// The host can install it, and waits for the person to ask.
    Idle,
    /// Its package is being downloaded.
    Downloading {
        /// Bytes received so far.
        received: u64,
        /// The package's size, when the server said.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(optional))]
        total: Option<u64>,
    },
    /// Downloaded and verified: restarting installs it.
    Ready,
    /// Being installed; the app restarts when it is done.
    Installing,
    /// It could not be downloaded, verified, or installed; `problem` says
    /// why. Asking again tries again.
    Failed {
        /// Why.
        problem: DiagnosticText,
    },
}
