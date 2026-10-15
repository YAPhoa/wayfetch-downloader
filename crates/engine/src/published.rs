//! Changes to published files on the user's request: renaming one, and
//! finding one for the host to open (architecture brief sections 7.7 and
//! 11.7).
//!
//! Both run on the coordinator, so the store's rename records are only
//! ever touched here or at startup: a rename recorded but unfinished when
//! another request arrives was left by a crash or a filesystem error, and
//! is reconciled before anything else is done with the job's file.

use std::path::PathBuf;
use std::time::SystemTime;

use wayfetch_domain::id::{JobId, RootId};
use wayfetch_domain::time::UtcTimestamp;
use wayfetch_protocol::{CommandEnvelope, CommandResult, ErrorCode, InstanceId, ProtocolError};

use crate::destinations::Destinations;
use crate::fs::{FsError, RenameCheck, Root};
use crate::store::{RenameIntent, RenameStart, Store, StoreError};

fn now() -> UtcTimestamp {
    UtcTimestamp::from_system_time(SystemTime::now())
}

/// The client-facing form of a refusal from the filesystem. Never names
/// the file, which the client already knows.
fn refusal(err: &FsError) -> ProtocolError {
    let (code, message) = match err {
        FsError::TargetExists(_) => (
            ErrorCode::AlreadyExists,
            "a file with that name is already in the folder",
        ),
        FsError::PublishedMissing(_) => (
            ErrorCode::NotApplicable,
            "the file is no longer in its folder",
        ),
        FsError::PublishedChanged(_) => (
            ErrorCode::NotApplicable,
            "the file was changed or replaced outside Wayfetch",
        ),
        FsError::Unsupported(_) => (
            ErrorCode::NotApplicable,
            "the file's folder does not support this",
        ),
        _ => (
            ErrorCode::NotApplicable,
            "the file's folder is unavailable; its drive may not be mounted",
        ),
    };
    ProtocolError::new(code, message)
}

/// The permitted root `id`, opened.
fn open(roots: &Destinations, instance: InstanceId, id: RootId) -> Result<Root, ProtocolError> {
    let root = roots.get(id).ok_or_else(|| {
        ProtocolError::new(
            ErrorCode::NotApplicable,
            "the file's folder is no longer a permitted destination",
        )
    })?;
    Root::open(&root.path, root.id, instance).map_err(|e| refusal(&e))
}

/// Finishes or drops an unfinished rename, judged by where the file is
/// now; `None` if it was dropped. One whose outcome cannot be told stays
/// recorded.
fn settle(
    store: &mut Store,
    roots: &Destinations,
    instance: InstanceId,
    intent: &RenameIntent,
) -> Result<Option<CommandResult>, ProtocolError> {
    let root = open(roots, instance, intent.root)?;
    match root
        .reconcile_rename(&intent.file, &intent.to)
        .map_err(|e| refusal(&e))?
    {
        RenameCheck::Renamed => Ok(Some(store.finish_rename(intent.job, now())?)),
        RenameCheck::NotRenamed => {
            store.abandon_rename(intent.job)?;
            Ok(None)
        }
    }
}

/// Reconciles every unfinished rename. One that cannot be reconciled now,
/// because its folder is unavailable, stays for a later attempt.
pub(crate) fn reconcile(store: &mut Store, roots: &Destinations, instance: InstanceId) {
    let Ok(pending) = store.pending_renames() else {
        return;
    };
    for intent in pending {
        let _ = settle(store, roots, instance, &intent);
    }
}

/// Runs a rename command: records it, renames the file, and finishes it.
/// If renaming fails partway, the outcome is judged by where the file is,
/// so a file that did get its new name is recorded under it.
pub(crate) fn rename(
    store: &mut Store,
    roots: &Destinations,
    instance: InstanceId,
    envelope: &CommandEnvelope,
) -> Result<CommandResult, ProtocolError> {
    reconcile(store, roots, instance);
    let intent = match store.begin_rename(envelope, instance, now())? {
        RenameStart::Done(result) => return Ok(result),
        RenameStart::Started(intent) => intent,
    };
    let root = match open(roots, instance, intent.root) {
        Ok(root) => root,
        // Nothing was touched.
        Err(e) => {
            store.abandon_rename(intent.job)?;
            return Err(e);
        }
    };
    match root.rename_published(&intent.file, &intent.to) {
        Ok(()) => Ok(store.finish_rename(intent.job, now())?),
        Err(e) => match settle(store, roots, instance, &intent)? {
            Some(result) => Ok(result),
            None => Err(refusal(&e)),
        },
    }
}

/// The absolute path of a listed, complete job's file, checked to still
/// hold the file Wayfetch published, for a host to open on its user's
/// request.
pub(crate) fn locate(
    store: &mut Store,
    roots: &Destinations,
    instance: InstanceId,
    job: JobId,
) -> Result<PathBuf, ProtocolError> {
    reconcile(store, roots, instance);
    let (root, file) = store.published_file(job).map_err(|e| match e {
        StoreError::NotFound("published file") => {
            ProtocolError::new(ErrorCode::NotApplicable, "the job has no file yet")
        }
        e => e.into(),
    })?;
    open(roots, instance, root)?
        .published_path(&file)
        .map_err(|e| refusal(&e))
}
