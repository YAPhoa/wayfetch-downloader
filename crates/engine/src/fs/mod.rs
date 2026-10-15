//! Destination roots, private staging, and publication (architecture brief
//! sections 7.6 and 7.7, ADR-16, ADR-17, ADR-29).
//!
//! # Roots are capabilities
//!
//! An operator configures roots by path, and the engine's store gives each
//! a stable [`RootId`] that its sentinel records. The engine opens a
//! [`Root`] once and from then on reaches files only through that open
//! directory handle with [`RelativePath`]s, using `cap-std`, which resolves
//! every path inside the handle: `..`, absolute paths, and symlinks that
//! lead outside the root fail instead of escaping it. Destination strings
//! from adapters or clients are never joined onto a host path.
//!
//! A root must carry a sentinel written by [`Root::initialize`]. Opening
//! fails closed when it is missing or names another root or instance, so an
//! unmounted bind mount is never mistaken for an empty destination and
//! nothing is written into the directory underneath (T30).
//! [`Root::verify`] repeats the check by path before publication, which
//! also catches a mount replaced after the root was opened.
//!
//! # Staging
//!
//! Each job generation writes to its own private file under
//! `.wayfetch/staging/` in the destination root, created exclusively and
//! never reused by another generation, so the final rename stays on one
//! filesystem. The file's identity (device and inode) is recorded with the
//! store's staging record. After a restart a partial is reopened only by
//! that record, and a missing, replaced, or shrunk file is a reconciliation
//! failure, never a reason to append blindly (T28). A file no generation
//! can reopen is removed by [`Root::remove_staging`] only while its name
//! still holds the recorded file; see the engine's cleanup module.
//!
//! # Publication
//!
//! [`Root::publish`] gives the synchronized staging file its final name
//! with a hard link, which fails instead of replacing an existing name on
//! every platform (a plain rename would replace it), synchronizes the
//! directory, then removes the staging name (ADR-16). The store
//! records a publication intent before and completes it after. After a
//! crash, [`Root::reconcile_publication`] compares both names with the
//! recorded identity and never deletes or overwrites a file it does not own
//! (T24, T31).
//!
//! # Platform support
//!
//! File identity and directory synchronization are implemented for Unix.
//! Elsewhere staging fails with [`FsError::Unsupported`] until the desktop
//! work adds them. Publication needs hard links in the destination root;
//! filesystems without them, and destinations that cross a mount point
//! below the root, are reported as unavailable rather than copied (T32 is
//! later work).

mod path;
mod publish;
mod root;
mod staging;
mod sys;

use std::io;

pub use path::{
    MAX_COMPONENT_BYTES, MAX_DEPTH, MAX_PATH_BYTES, PRIVATE_DIR, PathError, RelativePath,
};
pub use publish::PublicationCheck;
pub use root::Root;
#[cfg(test)]
pub(crate) use staging::tests::{root as test_root, staging_path};
pub use staging::{Removal, StagingFile, StagingRecord, SyncedRanges};
use wayfetch_domain::failure::FailureCode;
use wayfetch_domain::id::RootId;

/// What makes a staging file "the one this generation created" rather than
/// whatever now has its name: device and inode, plus the birth time where
/// the filesystem reports one, because an inode number freed by a deleted
/// file can be reused at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileIdentity {
    /// Device containing the file.
    pub device: u64,
    /// Inode number on that device.
    pub inode: u64,
    /// Birth time in nanoseconds since the Unix epoch, if known.
    pub birth_ns: Option<i64>,
}

impl FileIdentity {
    /// Whether `other` is the same file: same device and inode, and the
    /// same birth time when both observations have one.
    pub fn matches(&self, other: &Self) -> bool {
        self.device == other.device
            && self.inode == other.inode
            && match (self.birth_ns, other.birth_ns) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            }
    }
}

/// Why a filesystem operation failed.
#[derive(Debug, thiserror::Error)]
pub enum FsError {
    /// The root has no sentinel: it is unmounted, replaced by an empty
    /// directory, or was never initialized.
    #[error("{0} has no sentinel; it may be unmounted or uninitialized")]
    SentinelMissing(RootId),
    /// The root's sentinel belongs to another root or instance, or is
    /// unreadable.
    #[error("{0} carries the sentinel of another root or instance")]
    WrongRoot(RootId),
    /// The directory carries an unreadable sentinel or another instance's.
    #[error("the folder is marked for another Wayfetch instance")]
    ForeignSentinel,
    /// The directory at the root's path is no longer the one opened.
    #[error("{0} was replaced or remounted after it was opened")]
    RootReplaced(RootId),
    /// The path was rejected.
    #[error(transparent)]
    Path(#[from] PathError),
    /// The publication target already exists; nothing was overwritten.
    #[error("target {0} already exists")]
    TargetExists(RelativePath),
    /// The owned staging file is gone.
    #[error("staging file {0} is missing")]
    StagingMissing(String),
    /// Another file now has the staging file's name.
    #[error("staging file {0} was replaced")]
    StagingReplaced(String),
    /// The staging file is shorter than its durable coverage.
    #[error("staging file {name} has {len} bytes but {needed} are durable")]
    StagingShrunk {
        /// The staging name.
        name: String,
        /// Its current length.
        len: u64,
        /// The end of its durable coverage.
        needed: u64,
    },
    /// The platform lacks a required primitive.
    #[error("{0} is not supported on this platform")]
    Unsupported(&'static str),
    /// The filesystem reported an error.
    #[error("{op}: {source}")]
    Io {
        /// What was being done.
        op: &'static str,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
}

impl FsError {
    /// Wraps an I/O error from `op`.
    fn io(op: &'static str) -> impl FnOnce(io::Error) -> Self {
        move |source| Self::Io { op, source }
    }

    /// The failure to record for this error (failure matrix, section 7.4).
    /// Access refusals are told apart from unavailable destinations, and a
    /// damaged partial restarts rather than blocking the root (T29, T30).
    pub fn failure_code(&self) -> FailureCode {
        match self {
            Self::Io { source, .. } if source.kind() == io::ErrorKind::PermissionDenied => {
                FailureCode::PermissionDenied
            }
            Self::StagingMissing(_) | Self::StagingReplaced(_) | Self::StagingShrunk { .. } => {
                FailureCode::IntegrityMismatch
            }
            Self::Unsupported(_) => FailureCode::UnsupportedCapability,
            Self::SentinelMissing(_)
            | Self::WrongRoot(_)
            | Self::ForeignSentinel
            | Self::RootReplaced(_)
            | Self::Path(_)
            | Self::TargetExists(_)
            | Self::Io { .. } => FailureCode::DestinationUnavailable,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wayfetch_domain::failure::NextAction;

    #[test]
    fn errors_map_to_distinct_failures() {
        let io = |kind| FsError::Io {
            op: "test",
            source: io::Error::from(kind),
        };
        assert_eq!(
            io(io::ErrorKind::PermissionDenied).failure_code(),
            FailureCode::PermissionDenied
        );
        for kind in [
            io::ErrorKind::StorageFull,
            io::ErrorKind::ReadOnlyFilesystem,
            io::ErrorKind::NotFound,
        ] {
            assert_eq!(io(kind).failure_code(), FailureCode::DestinationUnavailable);
        }
        let root = RootId::from_u64(1).unwrap();
        assert_eq!(
            FsError::SentinelMissing(root).failure_code().next_action(),
            NextAction::BlockUntilRemediated
        );
        assert_eq!(
            FsError::StagingMissing("x".into())
                .failure_code()
                .next_action(),
            NextAction::RestartPartial
        );
    }
}
