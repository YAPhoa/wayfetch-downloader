//! Renaming a published file, and finding one for the host to open.
//!
//! Each request runs as its own task, with filesystem steps off the
//! coordinator and one request per job file at a time; others are refused as
//! busy. A rename left unfinished by a crash is reconciled at startup and
//! before the file is next used.

use std::path::PathBuf;
use std::time::SystemTime;

use tokio_util::sync::CancellationToken;
use wayfetch_domain::id::{JobId, RootId};
use wayfetch_domain::time::UtcTimestamp;
use wayfetch_protocol::{
    Command, CommandEnvelope, CommandResult, ErrorCode, InstanceId, ProtocolError,
};

use crate::destinations::{Claims, Destinations, PROBE_TIMEOUT, Probes, Slot};
use crate::fs::{FsError, RenameCheck, Root};
use crate::lifecycle::StoreClient;
use crate::store::{DeleteStart, RenameIntent, RenameStart, Store, StoreError};

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
        FsError::PublishedChanged(_) | FsError::StagingReplaced(_) => (
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
    /// Jobs whose file a request is working on.
    pub(crate) jobs: Claims<JobId>,
    pub(crate) roots: Destinations,
    pub(crate) instance: InstanceId,
    /// Once cancelled, no further step starts.
    pub(crate) cancel: CancellationToken,
}

fn shutting_down() -> ProtocolError {
    ProtocolError::new(ErrorCode::ShuttingDown, "engine is not running")
}

impl Files {
    /// Claims `job` for the whole of a request about its file, once
    /// another request about it ends, waiting a little for that.
    async fn claim(&self, job: JobId) -> Result<Slot<JobId>, ProtocolError> {
        self.jobs
            .claim_within(&job, PROBE_TIMEOUT)
            .await
            .ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::Busy,
                    "the file is being renamed or looked up; try again",
                )
            })
    }

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
        self.on_root_if_present(id, false, work)
            .await
            .map(|ran| ran.expect("a root that must be present was opened"))
    }

    /// Like [`Files::on_root`], but if `absent_ok` and the folder is gone
    /// (see [`FsError::is_absent_root`]), runs nothing and gives `None`.
    async fn on_root_if_present<T: Send + 'static>(
        &self,
        id: RootId,
        absent_ok: bool,
        work: impl FnOnce(&Root) -> Result<T, FsError> + Send + 'static,
    ) -> Result<Option<T>, Failure> {
        let root = self.roots.get(id).ok_or_else(|| {
            Failure::Untouched(ProtocolError::new(
                ErrorCode::NotApplicable,
                "the file's folder is no longer a permitted destination",
            ))
        })?;
        let slot = self.probes.claim_soon(&root.path).await.ok_or_else(|| {
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
                let opened = match Root::open(&root.path, root.id, instance) {
                    Ok(opened) => opened,
                    Err(e) if absent_ok && e.is_absent_root() => return Ok(None),
                    Err(e) => return Err(Failure::Untouched(refusal(&e))),
                };
                work(&opened).map(Some).map_err(|e| match e {
                    // Refused before anything was changed.
                    FsError::TargetExists(_) => Failure::Untouched(refusal(&e)),
                    e => Failure::Touched(refusal(&e)),
                })
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
    /// folder answers. The caller has claimed the job.
    async fn reconcile_job(&self, job: JobId) {
        if let Ok(Ok(Some(intent))) = self.store(move |store| store.pending_rename(job)).await {
            let _ = self.settle(&intent).await;
        }
    }

    /// Reconciles every unfinished rename that no request is working on.
    /// One that cannot be reconciled now, because its folder is
    /// unavailable or a request holds the job, stays for a later attempt.
    pub(crate) async fn reconcile(&self) {
        let Ok(Ok(pending)) = self.store(Store::pending_renames).await else {
            return;
        };
        for intent in pending {
            let Some(_slot) = self.jobs.claim(&intent.job) else {
                continue;
            };
            // Read again under the claim: a request may have finished it.
            if let Ok(Ok(Some(current))) = self
                .store(move |store| store.pending_rename(intent.job))
                .await
            {
                let _ = self.settle(&current).await;
            }
        }
    }

    /// Runs a rename command: records it, renames the file, and finishes
    /// it. If renaming fails partway, the outcome is judged by where the
    /// file is, so a file that did get its new name is recorded under it.
    pub(crate) async fn rename(
        &self,
        envelope: CommandEnvelope,
    ) -> Result<CommandResult, ProtocolError> {
        let _slot = match envelope.command {
            Command::Rename { job, .. } => {
                let slot = self.claim(job).await?;
                self.reconcile_job(job).await;
                Some(slot)
            }
            _ => None,
        };
        let instance = self.instance;
        let intent = match self
            .store(move |store| store.begin_rename(&envelope, instance, now()))
            .await??
        {
            RenameStart::Done(result) => return Ok(result),
            RenameStart::Started(intent) => intent,
        };
        self.recorded(&intent).await;
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

    /// Removes metadata by default, or explicitly authorized recorded data.
    /// The coordinator proved no download task still holds the job.
    pub(crate) async fn remove(
        &self,
        envelope: CommandEnvelope,
    ) -> Result<CommandResult, ProtocolError> {
        let id = match envelope.command {
            Command::Remove { job, .. } | Command::DeleteFiles { job, .. } => job,
            _ => {
                return Err(ProtocolError::new(
                    ErrorCode::Internal,
                    "expected removal command",
                ));
            }
        };
        let _slot = self.claim(id).await?;
        let instance = self.instance;
        if matches!(envelope.command, Command::Remove { .. }) {
            return self
                .store(move |store| store.execute(&envelope, instance, &Default::default(), now()))
                .await?;
        }
        let request = envelope.clone();
        let intent = match self
            .store(move |store| store.begin_delete(&request, instance))
            .await??
        {
            DeleteStart::Done(result) => return Ok(result),
            DeleteStart::Started(intent) => intent,
        };
        // A folder that is gone took its files with it: there is nothing
        // left to delete there.
        for (root_id, file) in intent.published {
            self.on_root_if_present(root_id, true, move |root| root.delete_published(&file))
                .await?;
        }
        for record in intent.staging {
            self.on_root_if_present(record.root, true, move |root| {
                root.verify()?;
                match root.remove_staging(&record)? {
                    crate::fs::Removal::Foreign => Err(FsError::StagingReplaced(record.name)),
                    _ => Ok(()),
                }
            })
            .await?;
        }
        self.store(move |store| store.finish_delete(id, &envelope, now()))
            .await?
            .map_err(Into::into)
    }

    /// The absolute path of a listed, complete job's file, checked to
    /// still hold the file Wayfetch published, for a host to open on its
    /// user's request.
    pub(crate) async fn locate(&self, job: JobId) -> Result<PathBuf, ProtocolError> {
        let _slot = self.claim(job).await?;
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

    /// A pause after a rename is recorded and before its file is touched,
    /// for tests: configured to pause, a rename in a folder named
    /// `stalled` waits there until the failpoint is removed.
    async fn recorded(&self, _intent: &RenameIntent) {
        #[cfg(feature = "failpoints")]
        if let Some(root) = self.roots.get(_intent.root) {
            let stalled = root.path.file_name() == Some(std::ffi::OsStr::new("stalled"));
            let _ = tokio::task::spawn_blocking(move || {
                fail::fail_point!("rename-recorded", stalled, |_| ());
            })
            .await;
        }
    }
}
