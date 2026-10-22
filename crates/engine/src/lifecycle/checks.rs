//! Checking complete downloads on the coordinator: checksum files are read,
//! files with checks waiting are hashed, and a file that failed is removed
//! so its job downloads it again, each as an engine task holding the job's
//! file claim. See the store's checks module for what is recorded.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::time::{Duration, Instant};

use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use wayfetch_domain::id::JobId;
use wayfetch_protocol::{CommandEnvelope, CommandResult, ErrorCode, InstanceId, ProtocolError};

use super::{Coordinator, Internal, StoreClient, now, shutting_down};
use crate::checksum::manifest::{self, MAX_BYTES};
use crate::checksum::{self, Hashing};
use crate::destinations::{Claims, Destinations, PROBE_TIMEOUT};
use crate::fs::{FsError, Root};
use crate::store::{CheckOutcome, ReplaceStart, Replacement};

/// How long a file whose folder could not be read waits before it is
/// tried again.
const RETRY_AFTER: Duration = Duration::from_secs(60);

/// Checks on or off, and the work running.
pub(super) struct FileChecks {
    /// Whether complete downloads are checked.
    pub(super) on: bool,
    /// Whether a file that fails is downloaded again once on its own.
    pub(super) redownload: bool,
    hashing: Hashing,
    /// Jobs with a task running.
    running: HashSet<JobId>,
    /// Jobs whose folder could not be read, and when to try again.
    waiting: HashMap<JobId, Instant>,
}

impl FileChecks {
    pub(super) fn new(on: bool, redownload: bool, hashing: Hashing) -> Self {
        Self {
            on,
            redownload,
            hashing,
            running: HashSet::new(),
            waiting: HashMap::new(),
        }
    }

    /// The engine's bound on files being hashed.
    pub(super) fn hashing(&self) -> Hashing {
        self.hashing.clone()
    }

    /// Jobs not to start work on now.
    fn busy(&mut self) -> Vec<JobId> {
        self.waiting.retain(|_, at| at.elapsed() < RETRY_AFTER);
        self.running
            .iter()
            .chain(self.waiting.keys())
            .copied()
            .collect()
    }
}

/// How a task ended.
pub(super) enum Ended {
    /// It is done, or has nothing to do.
    Done,
    /// The file's folder could not be read; try again later.
    Later,
}

/// What a task works with.
#[derive(Clone)]
struct Work {
    store: StoreClient,
    roots: Destinations,
    jobs: Claims<JobId>,
    hashing: Hashing,
    instance: InstanceId,
    cancel: CancellationToken,
}

/// Why a replacement did not finish.
enum NotReplaced {
    /// The folder could not be changed now; the replacement stays for a
    /// later attempt.
    Later(ProtocolError),
    /// It was refused or answered; nothing is left to do.
    Final(ProtocolError),
}

impl From<NotReplaced> for ProtocolError {
    fn from(e: NotReplaced) -> Self {
        match e {
            NotReplaced::Later(e) | NotReplaced::Final(e) => e,
        }
    }
}

/// Why reading a published file failed.
enum ReadError {
    /// The file is gone, or changed or replaced outside Wayfetch.
    Changed(&'static str),
    /// The folder could not be read, or the engine is stopping.
    Unavailable,
}

impl From<FsError> for ReadError {
    fn from(e: FsError) -> Self {
        match e {
            FsError::PublishedMissing(_) => Self::Changed("The file is no longer in its folder."),
            FsError::PublishedChanged(_) => {
                Self::Changed("The file was changed or replaced outside Wayfetch.")
            }
            _ => Self::Unavailable,
        }
    }
}

impl Work {
    async fn store<R: Send + 'static>(
        &self,
        work: impl FnOnce(&mut crate::store::Store) -> R + Send + 'static,
    ) -> Option<R> {
        if self.cancel.is_cancelled() {
            return None;
        }
        self.store.call(work).await
    }

    /// Reads `job`'s checksum file and records what it lists.
    async fn read_checksum_file(&self, plan: crate::store::ManifestPlan) -> Ended {
        let Some(root) = self.roots.get(plan.root) else {
            return Ended::Later;
        };
        let (instance, cancel) = (self.instance, self.cancel.clone());
        let (file, modified) = (plan.file.clone(), plan.modified_ns);
        let read = self
            .hashing
            .run(move || -> Result<Vec<u8>, ReadError> {
                if file.size > MAX_BYTES {
                    return Ok(Vec::new());
                }
                let root = Root::open(&root.path, root.id, instance)?;
                let (opened, _) = root.open_published(&file, modified)?;
                let mut bytes = Vec::new();
                opened
                    .take(MAX_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|_| ReadError::Unavailable)?;
                if cancel.is_cancelled() {
                    return Err(ReadError::Unavailable);
                }
                Ok(bytes)
            })
            .await;
        let read = match read {
            None | Some(Err(ReadError::Unavailable)) => return Ended::Later,
            Some(Err(ReadError::Changed(why))) => Err(why.to_owned()),
            Some(Ok(_)) if plan.file.size > MAX_BYTES => {
                Err(manifest::ManifestError::TooLarge.to_string())
            }
            Some(Ok(bytes)) => manifest::parse(plan.kind, &bytes).map_err(|e| e.to_string()),
        };
        let (job, publication) = (plan.job, plan.publication);
        self.store(move |store| store.record_checksum_file(job, publication, read, now()))
            .await;
        Ended::Done
    }

    /// Hashes `job`'s published file for its waiting checks, records the
    /// outcome, and carries out the replacement that authorized, if any.
    async fn check(&self, job: JobId) -> Ended {
        let Some(Ok(Some(plan))) = self.store(move |store| store.check_plan(job)).await else {
            return Ended::Done;
        };
        let Some(root) = self.roots.get(plan.root) else {
            return Ended::Later;
        };
        let (instance, cancel) = (self.instance, self.cancel.clone());
        let (file, modified, algorithms) =
            (plan.file.clone(), plan.modified_ns, plan.algorithms.clone());
        let hashed = self
            .hashing
            .run(move || -> Result<CheckOutcome, ReadError> {
                let root = Root::open(&root.path, root.id, instance)?;
                let (mut opened, before) = root.open_published(&file, modified)?;
                let digests = checksum::hash(&mut opened, &algorithms, &cancel)
                    .map_err(|_| ReadError::Unavailable)?
                    .ok_or(ReadError::Unavailable)?;
                // Changed while it was read: the hashes say nothing.
                if Root::file_version(&opened)? != before {
                    return Err(ReadError::Changed(
                        "The file was changed outside Wayfetch while it was checked.",
                    ));
                }
                Ok(CheckOutcome::Hashed(before, digests))
            })
            .await;
        let outcome = match hashed {
            None | Some(Err(ReadError::Unavailable)) => return Ended::Later,
            Some(Err(ReadError::Changed(why))) => CheckOutcome::Changed(why),
            Some(Ok(outcome)) => outcome,
        };
        let publication = plan.publication;
        let Some(Ok(replacement)) = self
            .store(move |store| store.record_file_check(job, publication, outcome, now()))
            .await
        else {
            return Ended::Done;
        };
        match replacement {
            Some(replacement) => match self.replace(replacement).await {
                Err(NotReplaced::Later(_)) => Ended::Later,
                _ => Ended::Done,
            },
            None => Ended::Done,
        }
    }

    /// Removes the file of an authorized replacement, while it is unchanged,
    /// and finishes it; a changed file is kept and the replacement dropped.
    /// Answers a person's command result, if one asked. Removing is not
    /// hashing, so it does not wait for a hashing permit.
    async fn replace(&self, plan: Replacement) -> Result<Option<CommandResult>, NotReplaced> {
        let later =
            |message| NotReplaced::Later(ProtocolError::new(ErrorCode::NotApplicable, message));
        let root = self
            .roots
            .get(plan.root)
            .ok_or_else(|| later("the file's folder is no longer a permitted destination"))?;
        let instance = self.instance;
        let (file, modified) = (plan.file.clone(), plan.modified_ns);
        let removed = self
            .hashing
            .run_unbounded(move || {
                Root::open(&root.path, root.id, instance)?.retire_published(&file, modified)
            })
            .await
            .ok_or_else(|| NotReplaced::Later(shutting_down()))?;
        let job = plan.job;
        fn stored<T>(
            answer: Option<Result<T, crate::store::StoreError>>,
        ) -> Result<T, NotReplaced> {
            match answer {
                Some(Ok(answer)) => Ok(answer),
                Some(Err(e)) => Err(NotReplaced::Final(e.into())),
                None => Err(NotReplaced::Later(shutting_down())),
            }
        }
        match removed {
            Ok(()) => stored(
                self.store(move |store| store.finish_replacement(job, now()))
                    .await,
            ),
            Err(FsError::PublishedChanged(_)) => {
                let why = "The file was changed outside Wayfetch, so it was not removed.";
                stored(
                    self.store(move |store| store.refuse_replacement(job, why, now()))
                        .await,
                )?;
                Err(NotReplaced::Final(ProtocolError::new(
                    ErrorCode::NotApplicable,
                    "the file was changed outside Wayfetch, so it was not removed",
                )))
            }
            Err(_) => Err(later(
                "the file's folder is unavailable; its drive may not be mounted",
            )),
        }
    }

    /// Finishes `job`'s unfinished replacement.
    async fn resume_replacement(&self, job: JobId) -> Ended {
        match self.store(move |store| store.replacement(job)).await {
            Some(Ok(Some(plan))) => match self.replace(plan).await {
                Err(NotReplaced::Later(_)) => Ended::Later,
                _ => Ended::Done,
            },
            _ => Ended::Done,
        }
    }

    /// A person's retry of a complete job whose file failed its check.
    async fn retry(&self, envelope: CommandEnvelope) -> Result<CommandResult, ProtocolError> {
        let job = match envelope.command {
            wayfetch_protocol::Command::Retry { job, .. } => job,
            _ => return Err(ProtocolError::new(ErrorCode::Internal, "expected retry")),
        };
        let _slot = self
            .jobs
            .claim_within(&job, PROBE_TIMEOUT)
            .await
            .ok_or_else(|| {
                ProtocolError::new(
                    ErrorCode::Busy,
                    "the file is being checked, renamed, or looked up; try again",
                )
            })?;
        let instance = self.instance;
        let plan = match self
            .store(move |store| store.begin_replacement(&envelope, instance, now()))
            .await
            .ok_or_else(shutting_down)??
        {
            ReplaceStart::Done(result) => return Ok(result),
            ReplaceStart::Started(plan) => plan,
        };
        self.replace(plan)
            .await?
            .ok_or_else(|| ProtocolError::new(ErrorCode::Internal, "the retry was not recorded"))
    }
}

impl Coordinator {
    fn check_work(&self) -> Work {
        Work {
            store: self.client.clone(),
            roots: self.destinations.clone(),
            jobs: self.file_jobs.clone(),
            hashing: self.file_checks.hashing.clone(),
            instance: self.identity.instance,
            cancel: self.supervisor.cancel.clone(),
        }
    }

    /// After a download ended: plans checks of the file it completed.
    pub(super) async fn plan_checks(&mut self, job: JobId) {
        if self.file_checks.on {
            let _ = self
                .with_store(move |store| store.plan_checks(job, now()))
                .await;
        }
    }

    /// Starts tasks for unfinished replacements, checksum files to read,
    /// and files to check, at most [`checksum::MAX_HASHING`] of the last
    /// two at once.
    pub(super) async fn run_file_checks(&mut self) {
        let busy = self.file_checks.busy();
        let Ok(replacements) = self.with_store(|store| store.pending_replacements()).await else {
            return;
        };
        for job in replacements.into_iter().filter(|j| !busy.contains(j)) {
            self.spawn_check(job, |work, job| async move {
                work.resume_replacement(job).await
            });
        }
        let free = checksum::MAX_HASHING.saturating_sub(self.file_checks.running.len());
        if !self.file_checks.on || free == 0 {
            return;
        }
        let busy = self.file_checks.busy();
        let excluded = busy.clone();
        let Ok(files) = self
            .with_store(move |store| store.pending_checksum_files(free, &excluded))
            .await
        else {
            return;
        };
        let free = free.saturating_sub(files.len());
        for plan in files {
            self.spawn_check(plan.job, move |work, _| async move {
                work.read_checksum_file(plan).await
            });
        }
        if free == 0 {
            return;
        }
        let excluded = self.file_checks.busy();
        let Ok(jobs) = self
            .with_store(move |store| store.pending_checks(free, &excluded))
            .await
        else {
            return;
        };
        for job in jobs {
            self.spawn_check(job, |work, job| async move { work.check(job).await });
        }
    }

    /// Runs `task` for `job` as an engine task holding the job's file
    /// claim, then tells the coordinator it ended.
    fn spawn_check<F, Fut>(&mut self, job: JobId, task: F)
    where
        F: FnOnce(Work, JobId) -> Fut + Send + 'static,
        Fut: Future<Output = Ended> + Send + 'static,
    {
        self.file_checks.running.insert(job);
        let work = self.check_work();
        let finished = self.client.0.clone();
        self.supervisor.spawn(async move {
            let ended = match work.jobs.claim_within(&job, PROBE_TIMEOUT).await {
                Some(_slot) => task(work, job).await,
                None => Ended::Later,
            };
            let _ = finished.send(Internal::FileCheckEnded(job, ended)).await;
        });
    }

    /// A task ended: an archive waiting for it may be unpacked now.
    pub(super) async fn file_check_ended(&mut self, job: JobId, ended: Ended) {
        self.file_checks.running.remove(&job);
        if let Ended::Later = ended {
            self.file_checks.waiting.insert(job, Instant::now());
        }
        self.plan_extractions(job).await;
    }

    /// Runs a person's retry of a complete job whose file failed its check.
    pub(super) fn retry_replacement(
        &mut self,
        envelope: Box<CommandEnvelope>,
        reply: oneshot::Sender<Result<CommandResult, ProtocolError>>,
    ) {
        let work = self.check_work();
        self.answer(reply, async move { work.retry(*envelope).await });
    }
}
