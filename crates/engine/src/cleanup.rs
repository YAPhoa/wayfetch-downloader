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
//! so the name is never taken for the engine's own again. Every step can
//! be interrupted: a pass after a crash finds the record with or without
//! its file and finishes the job, so cleanup is safe to repeat.
//!
//! A crash between creating a staging file and registering it, or a
//! registration refused because the job moved on meanwhile, leaves a file
//! no record names. With `unrecorded`, a pass also removes such a file if
//! it has the exact name the engine gives the file of an older generation
//! of an existing job, no record or publication names it, and it is a
//! regular file with no other name. Everything else in the staging
//! directory, and everything outside it, is left alone.

use crate::fs::{FsError, Removal, Root};
use crate::store::{Store, StoreError};

/// How many records or names a pass handles per store query.
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

/// Removes the abandoned staging files of `root` and, with `unrecorded`,
/// the files the engine left in it without a record. Blocking.
///
/// A file of an older generation than its job's current one can no longer
/// be registered (the store rejects the stale generation), so removing an
/// unrecorded one never races a registration. Listing the staging directory
/// is a pass over every entry, so the engine does that part only at startup.
pub(crate) fn sweep(
    store: &mut Store,
    root: &Root,
    unrecorded: bool,
) -> Result<Swept, CleanupError> {
    let mut swept = Swept::default();
    loop {
        let batch = store.abandoned_staging(root.id(), BATCH)?;
        if batch.is_empty() {
            break;
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
    if unrecorded {
        let mut names = root.staging_names()?.peekable();
        while names.peek().is_some() {
            let batch = names.by_ref().take(BATCH).collect();
            for name in store.unrecorded_staging(root.id(), batch)? {
                if root.remove_unrecorded_staging(&name)? == Removal::Removed {
                    swept.removed += 1;
                }
            }
        }
    }
    Ok(swept)
}
