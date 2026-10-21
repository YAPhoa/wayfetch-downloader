//! Removal of staging files no job generation can reopen.
//!
//! The store says which files are abandoned. Each is removed only while its
//! name still holds the recorded file, and its record only after the removal
//! is durable, so a pass is safe to repeat after a crash. A file no record
//! names is never removed.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::destinations::Probes;
use crate::fs::{FsError, Removal, Root, StagingRecord};
use crate::lifecycle::StoreClient;
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
    /// The engine stopped.
    #[error("the engine stopped")]
    Stopped,
}

/// The next abandoned records of `root`. Store only.
fn abandoned(store: &mut Store, root: &Root) -> Result<Vec<StagingRecord>, StoreError> {
    store.abandoned_staging(root.id(), BATCH)
}

/// Removes the files of `batch`, stopping at the first refusal. Returns
/// what each removal found, and the refusal. Filesystem only.
fn remove(
    root: &Root,
    batch: Vec<StagingRecord>,
) -> (Vec<(StagingRecord, Removal)>, Option<FsError>) {
    let mut done = Vec::with_capacity(batch.len());
    for record in batch {
        match root.remove_staging(&record) {
            Ok(found) => {
                if found == Removal::Removed {
                    crash_point!("cleanup-before-forget");
                }
                done.push((record, found));
            }
            Err(e) => return (done, Some(e)),
        }
    }
    (done, None)
}

/// Records what the removals found, once they are durable. Store only.
fn forget(
    store: &mut Store,
    done: &[(StagingRecord, Removal)],
    swept: &mut Swept,
) -> Result<(), StoreError> {
    for (record, found) in done {
        store.finish_staging_cleanup(record, *found)?;
        match found {
            Removal::Removed => swept.removed += 1,
            Removal::Missing => {}
            Removal::Foreign => swept.foreign += 1,
        }
    }
    Ok(())
}

/// Removes the abandoned staging files of `root`, with the store and the
/// folder used in turn: a folder that hangs stalls only this pass, never
/// the coordinator. Removals run through `probes`, holding the state
/// directory until they end, and none starts once `cancel` fires.
pub(crate) async fn sweep(
    store: &StoreClient,
    probes: &Probes,
    cancel: &CancellationToken,
    root: Arc<Root>,
) -> Result<Swept, CleanupError> {
    let mut swept = Swept::default();
    loop {
        if cancel.is_cancelled() {
            return Err(CleanupError::Stopped);
        }
        let opened = root.clone();
        let batch = store
            .call(move |store| abandoned(store, &opened))
            .await
            .ok_or(CleanupError::Stopped)??;
        if batch.is_empty() {
            return Ok(swept);
        }
        if cancel.is_cancelled() {
            return Err(CleanupError::Stopped);
        }
        let opened = root.clone();
        let (done, refused) = probes
            .blocking(move || remove(&opened, batch))
            .await
            .map_err(|_| CleanupError::Stopped)?;
        let (forgotten, counted) = store
            .call(move |store| {
                let mut counted = Swept::default();
                (forget(store, &done, &mut counted), counted)
            })
            .await
            .ok_or(CleanupError::Stopped)?;
        swept.removed += counted.removed;
        swept.foreign += counted.foreign;
        forgotten?;
        if let Some(e) = refused {
            return Err(e.into());
        }
    }
}

/// [`sweep`] with the store held throughout. Blocking; for tests.
#[cfg(test)]
pub(crate) fn sweep_now(store: &mut Store, root: &Root) -> Result<Swept, CleanupError> {
    let mut swept = Swept::default();
    loop {
        let batch = abandoned(store, root)?;
        if batch.is_empty() {
            return Ok(swept);
        }
        let (done, refused) = remove(root, batch);
        forget(store, &done, &mut swept)?;
        if let Some(e) = refused {
            return Err(e.into());
        }
    }
}
