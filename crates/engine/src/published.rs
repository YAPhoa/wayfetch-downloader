//! Changes to published files on the user's request: renaming one, and
//! finding one for the host to open (architecture brief sections 7.7 and
//! 11.7).
//!
//! Each request runs as a task of its own. The store records and finishes
//! the change on the coordinator, and the filesystem steps run between, on
//! blocking threads, one at a time per root folder ([`Probes`]), so a
//! folder that hangs delays only the requests about it, and is reported as
//! busy to the ones after. A step is never abandoned once started; at
//! shutdown the request stops before its next one.
//!
//! A rename recorded but unfinished was left by a crash or a filesystem
//! error. It is reconciled at startup, and again before anything else is
//! done with the job's file, judged by where the file is now.

use std::path::PathBuf;
use std::time::SystemTime;

use tokio_util::sync::CancellationToken;
use wayfetch_domain::id::{JobId, RootId};
use wayfetch_domain::time::UtcTimestamp;
use wayfetch_protocol::{
    Command, CommandEnvelope, CommandResult, ErrorCode, InstanceId, ProtocolError,
};

use crate::destinations::{Destinations, Probes};
use crate::fs::{FsError, RenameCheck, Root};
use crate::lifecycle::StoreClient;
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

/// Why work on a root folder failed: before anything in it was touched, or
/// after it may have been.
enum Failure {
    Untouched(ProtocolError),
    Touched(ProtocolError),
}

impl From<Failure> for ProtocolError {
    fn from(failure: Failure) -> Self {
        match failure {
            Failure::Untouched(e) | Failure::Touched(e) => e,
        }
    }
}

/// What a request about published files works with: the store through
/// the coordinator, and the permitted roots as they were when it arrived.
pub(crate) struct Files {
    pub(crate) store: StoreClient,
    pub(crate) probes: Probes,
    pub(crate) roots: Destinations,
    pub(crate) instance: InstanceId,
    /// Once cancelled, no further step starts.
    pub(crate) cancel: CancellationToken,
}

fn shutting_down() -> ProtocolError {
    ProtocolError::new(ErrorCode::ShuttingDown, "engine is not running")
}

impl Files {
    /// Runs `work` on the store.
    async fn store<R: Send + 'static>(
        &self,
        work: impl FnOnce(&mut Store) -> R + Send + 'static,
    ) -> Result<R, ProtocolError> {
        if self.cancel.is_cancelled() {
            return Err(shutting_down());
        }
        self.store.call(work).await.ok_or_else(shutting_down)
    }

    /// Opens root `id` and runs `work` on it, on a blocking thread, unless
    /// other work on the folder is still running or the engine is stopping.
    /// The work is waited for, holding the state directory, since it cannot
    /// be interrupted.
    async fn on_root<T: Send + 'static>(
        &self,
        id: RootId,
        work: impl FnOnce(&Root) -> Result<T, FsError> + Send + 'static,
    ) -> Result<T, Failure> {
        let root = self.roots.get(id).ok_or_else(|| {
            Failure::Untouched(ProtocolError::new(
                ErrorCode::NotApplicable,
                "the file's folder is no longer a permitted destination",
            ))
        })?;
        let slot = self.probes.claim(&root.path).ok_or_else(|| {
            Failure::Untouched(ProtocolError::new(
                ErrorCode::Busy,
                "the file's folder is busy or not responding; try again",
            ))
        })?;
        if self.cancel.is_cancelled() {
            return Err(Failure::Untouched(shutting_down()));
        }
        let instance = self.instance;
        let ran = self
            .probes
            .blocking(move || {
                let _slot = slot;
                let opened = Root::open(&root.path, root.id, instance)
                    .map_err(|e| Failure::Untouched(refusal(&e)))?;
                work(&opened).map_err(|e| Failure::Touched(refusal(&e)))
            })
            .await;
        ran.unwrap_or_else(|_| {
            Err(Failure::Touched(ProtocolError::new(
                ErrorCode::Internal,
                "the file's folder could not be changed",
            )))
        })
    }

    /// Finishes or drops an unfinished rename, judged by where the file is
    /// now; `None` if it was dropped. One whose outcome cannot be told
    /// stays recorded.
    async fn settle(&self, intent: &RenameIntent) -> Result<Option<CommandResult>, ProtocolError> {
        let (job, file, to) = (intent.job, intent.file.clone(), intent.to.clone());
        match self
            .on_root(intent.root, move |root| root.reconcile_rename(&file, &to))
            .await?
        {
            RenameCheck::Renamed => Ok(Some(
                self.store(move |store| store.finish_rename(job, now()))
                    .await??,
            )),
            RenameCheck::NotRenamed => {
                self.store(move |store| store.abandon_rename(job)).await??;
                Ok(None)
            }
        }
    }

    /// Reconciles the job's unfinished rename, if it has one and its
    /// folder answers.
    async fn reconcile_job(&self, job: JobId) {
        if let Ok(Ok(Some(intent))) = self.store(move |store| store.pending_rename(job)).await {
            let _ = self.settle(&intent).await;
        }
    }

    /// Reconciles every unfinished rename. One that cannot be reconciled
    /// now, because its folder is unavailable, stays for a later attempt.
    pub(crate) async fn reconcile(&self) {
        let Ok(Ok(pending)) = self.store(Store::pending_renames).await else {
            return;
        };
        for intent in pending {
            let _ = self.settle(&intent).await;
        }
    }

    /// Runs a rename command: records it, renames the file, and finishes
    /// it. If renaming fails partway, the outcome is judged by where the
    /// file is, so a file that did get its new name is recorded under it.
    pub(crate) async fn rename(
        &self,
        envelope: CommandEnvelope,
    ) -> Result<CommandResult, ProtocolError> {
        if let Command::Rename { job, .. } = envelope.command {
            self.reconcile_job(job).await;
        }
        let instance = self.instance;
        let intent = match self
            .store(move |store| store.begin_rename(&envelope, instance, now()))
            .await??
        {
            RenameStart::Done(result) => return Ok(result),
            RenameStart::Started(intent) => intent,
        };
        let (job, file, to) = (intent.job, intent.file.clone(), intent.to.clone());
        match self
            .on_root(intent.root, move |root| root.rename_published(&file, &to))
            .await
        {
            Ok(()) => Ok(self
                .store(move |store| store.finish_rename(job, now()))
                .await??),
            Err(Failure::Untouched(e)) => {
                self.store(move |store| store.abandon_rename(job)).await??;
                Err(e)
            }
            Err(Failure::Touched(e)) => match self.settle(&intent).await {
                Ok(Some(result)) => Ok(result),
                _ => Err(e),
            },
        }
    }

    /// The absolute path of a listed, complete job's file, checked to
    /// still hold the file Wayfetch published, for a host to open on its
    /// user's request.
    pub(crate) async fn locate(&self, job: JobId) -> Result<PathBuf, ProtocolError> {
        self.reconcile_job(job).await;
        let (root, file) = self
            .store(move |store| store.published_file(job))
            .await?
            .map_err(|e| match e {
                StoreError::NotFound("published file") => {
                    ProtocolError::new(ErrorCode::NotApplicable, "the job has no file yet")
                }
                e => e.into(),
            })?;
        Ok(self
            .on_root(root, move |root| root.published_path(&file))
            .await?)
    }
}
