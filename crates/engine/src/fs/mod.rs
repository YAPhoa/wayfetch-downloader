//! Destination roots, private staging, and crash-safe publication (ADR-16,
//! ADR-17, ADR-29).
//!
//! A [`Root`] is an open directory handle reached only through
//! [`RelativePath`]s, so nothing escapes it. It must carry this profile's
//! marker, so an unmounted folder fails closed instead of being written
//! into. Each job generation writes to its own staging file, reopened after
//! a restart only by its recorded identity. Publication and renames use
//! no-clobber hard links between store intents, so a crash never overwrites
//! a file Wayfetch does not own.
//!
//! Unix only for now; elsewhere staging reports [`FsError::Unsupported`].

mod extract;
mod path;
mod publish;
mod root;
mod staging;
mod sys;

use std::io;

pub use extract::{RESERVE_BYTES, Tally, TreeError};
pub use path::{
    MAX_COMPONENT_BYTES, MAX_DEPTH, MAX_PATH_BYTES, PRIVATE_DIR, PathError, RelativePath,
};
pub use publish::{PublicationCheck, PublishedFile, RenameCheck};
pub use root::Root;
#[cfg(test)]
pub(crate) use staging::tests::{own_name, root as test_root, staging_path};
pub use staging::{Removal, StagingFile, StagingRecord, SyncedRanges};
use wayfetch_domain::failure::FailureCode;
use wayfetch_domain::id::RootId;

/// The step that opens a root's folder by its path.
const OPEN_ROOT: &str = "open root";

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
    /// The root has no marker of this profile: it is unmounted, replaced by
    /// an empty directory, or was never set up by this profile.
    #[error("{0} has no Wayfetch marker; it may be unmounted or not set up")]
    MarkerMissing(RootId),
    /// This profile's marker in the folder names another of its roots.
    #[error("the folder of {0} is marked as another download folder")]
    WrongRoot(RootId),
    /// This profile's marker in the folder cannot be read.
    #[error("the folder's Wayfetch marker is damaged")]
    UnreadableMarker,
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
    /// The published file is no longer at its name.
    #[error("{0} is no longer in the folder")]
    PublishedMissing(RelativePath),
    /// Another file, or the published file at another size, has the
    /// published file's name: it was replaced or changed outside Wayfetch.
    #[error("{0} was changed or replaced outside Wayfetch")]
    PublishedChanged(RelativePath),
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
    /// Whether opening a root failed because the folder is gone: it no
    /// longer exists, or it no longer holds this profile's marker, as when its
    /// contents were deleted. An empty mount point looks the same.
    pub fn is_absent_root(&self) -> bool {
        match self {
            Self::MarkerMissing(_) => true,
            Self::Io {
                op: OPEN_ROOT,
                source,
            } => source.kind() == io::ErrorKind::NotFound,
            _ => false,
        }
    }

    /// Wraps an I/O error from `op`.
    fn io(op: &'static str) -> impl FnOnce(io::Error) -> Self {
        move |source| Self::Io { op, source }
    }

    /// The failure to record for this error.
    /// Access refusals are told apart from unavailable destinations, and a
    /// damaged partial restarts rather than blocking the root.
    pub fn failure_code(&self) -> FailureCode {
        match self {
            Self::Io { source, .. } if source.kind() == io::ErrorKind::PermissionDenied => {
                FailureCode::PermissionDenied
            }
            Self::StagingMissing(_)
            | Self::StagingReplaced(_)
            | Self::StagingShrunk { .. }
            | Self::PublishedMissing(_)
            | Self::PublishedChanged(_) => FailureCode::IntegrityMismatch,
            Self::Unsupported(_) => FailureCode::UnsupportedCapability,
            Self::MarkerMissing(_)
            | Self::WrongRoot(_)
            | Self::UnreadableMarker
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
            FsError::MarkerMissing(root).failure_code().next_action(),
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
