//! Removal of staging files no job generation can reopen (architecture
//! brief section 7.6).
//!
//! The store says which files are abandoned: a newer generation of the job
//! registered its own file, or the job went back to the queue to start from
//! zero. A file the job may still continue (a paused, blocked, failed, or
//! cancelled job's newest partial, or one a restart may adopt) is never
//! abandoned, and neither is one an unfinished publication names.
//!
//! Each abandoned file is removed only if its name still holds the recorded
//! file, and the record is deleted only after the removal is durable. A
//! name holding anything else is left in place and its record kept, marked
//! so later passes leave it alone. Every step can
//! be interrupted: a pass after a crash finds the record with or without
//! its file and finishes the job, so cleanup is safe to repeat.
//!
//! Only a recorded identity proves the engine created a file, so a file no
//! record names is never removed, whatever its name. A crash between
//! creating a staging file and registering it leaves such a file in place;
//! a download whose registration is refused removes its own file, whose
//! identity it still holds.

use crate::fs::{FsError, Removal, Root};
use crate::store::{Store, StoreError};

/// How many records a pass handles per store query.
const BATCH: usize = 64;

/// What a cleanup pass did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Swept {
    /// Staging files removed.
    pub(crate) removed: usize,
    /// Abandoned names found holding another file, left in place.
    pub(crate) foreign: usize,
}

/// Why a cleanup pass stopped early. What it had done is durable, and the
/// next pass continues from there.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CleanupError {
    /// The filesystem refused a step.
    #[error(transparent)]
    Fs(#[from] FsError),
    /// The store refused a step.
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Removes the abandoned staging files of `root`. Blocking.
pub(crate) fn sweep(store: &mut Store, root: &Root) -> Result<Swept, CleanupError> {
    let mut swept = Swept::default();
    loop {
        let batch = store.abandoned_staging(root.id(), BATCH)?;
        if batch.is_empty() {
            return Ok(swept);
        }
        for record in batch {
            let found = root.remove_staging(&record)?;
            match found {
                Removal::Removed => {
                    swept.removed += 1;
                    crash_point!("cleanup-before-forget");
                }
                Removal::Missing => {}
                Removal::Foreign => swept.foreign += 1,
            }
            store.finish_staging_cleanup(&record, found)?;
        }
    }
}
