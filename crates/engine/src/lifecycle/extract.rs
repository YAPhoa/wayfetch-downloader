//! Unpacking archives on the coordinator (protocol 6.0): one extraction at
//! a time, started after each state change while the extractor can be
//! used, and archives queued as their downloads complete when the user
//! has it on. See the extract module for what an attempt guarantees.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;
use wayfetch_domain::id::JobId;
use wayfetch_protocol::media::PluginSummary;
use wayfetch_protocol::{ErrorCode, ProtocolError};

use super::{Coordinator, Internal, StoreClient, now};
use crate::accounts::Secret;
use crate::destinations::{Destination, Probes};
use crate::extract::{self, Archive, ExtractError, Extractor, Layout};
use crate::fs::{FsError, RESERVE_BYTES, Root, TreeError};
use crate::store::{ExtractionEnd, ExtractionStart};

/// How long a password given for an archive is kept for its next attempt.
const PASSWORD_TTL: Duration = Duration::from_secs(10 * 60);

/// How often an attempt's output is measured while it is written.
const WATCH_INTERVAL: Duration = Duration::from_secs(1);

/// The extractor and the attempt running.
#[derive(Default)]
pub(super) struct Extractions {
    extractor: Option<Arc<dyn Extractor>>,
    /// The attempt running, and what stops it.
    running: Option<(JobId, CancellationToken)>,
    /// Whether archives are queued as their downloads complete.
    pub(super) automatic: bool,
    /// Passwords given for archives' next attempts, until they expire.
    passwords: HashMap<JobId, (Secret, Instant)>,
}

impl Extractions {
    pub(super) fn new(extractor: Option<Arc<dyn Extractor>>, automatic: bool) -> Self {
        Self {
            extractor,
            automatic,
            ..Self::default()
        }
    }

    /// The extractor's plugin, if one is registered.
    pub(super) fn plugin(&self) -> Option<PluginSummary> {
        self.extractor.as_ref().map(|e| e.plugin())
    }

    /// Why archives cannot be unpacked now, if they cannot: no extractor,
    /// or one turned off or not working.
    pub(super) fn unavailable(&self) -> Option<ProtocolError> {
        let why = match &self.extractor {
            None => "archives cannot be unpacked here".to_owned(),
            Some(e) if !e.ready() => format!(
                "archives cannot be unpacked while {} is turned off or not working",
                e.plugin().name
            ),
            Some(_) => return None,
        };
        Some(ProtocolError::new(ErrorCode::NotApplicable, &why))
    }

    /// Keeps `password` for `job`'s next attempt.
    pub(super) fn give_password(&mut self, job: JobId, password: Secret) {
        self.passwords
            .retain(|_, (_, at)| at.elapsed() < PASSWORD_TTL);
        self.passwords.insert(job, (password, Instant::now()));
    }

    /// Stops `job`'s attempt, if it is running.
    pub(super) fn stop(&self, job: JobId) {
        if let Some((running, cancel)) = &self.running
            && *running == job
        {
            cancel.cancel();
        }
    }
}

impl Coordinator {
    /// After a download ended: queues the archive it completed, if the
    /// user has extraction on and archives can be unpacked, and checks
    /// archives waiting for volumes.
    pub(super) async fn plan_extractions(&mut self, job: JobId) {
        let automatic = self.extractions.automatic && self.extractions.unavailable().is_none();
        let _ = self
            .with_store(move |store| store.plan_extractions(job, automatic, now()))
            .await;
    }

    /// Starts the next queued extraction, unless one is running or the
    /// extractor cannot be used.
    pub(super) async fn admit_extraction(&mut self) {
        let extractor = match &self.extractions.extractor {
            Some(e) if self.extractions.running.is_none() && e.ready() => e.clone(),
            _ => return,
        };
        let Ok(Some(start)) = self.with_store(|store| store.next_extraction(now())).await else {
            return;
        };
        let password = self
            .extractions
            .passwords
            .remove(&start.job)
            .filter(|(_, at)| at.elapsed() < PASSWORD_TTL)
            .map(|(password, _)| password);
        let cancel = self.supervisor.cancel.child_token();
        self.extractions.running = Some((start.job, cancel.clone()));
        let attempt = Attempt {
            store: self.client.clone(),
            fs: Probes::new(self.supervisor.guard()),
            root: self.destinations.get(start.root),
            instance: self.identity.instance,
            extractor,
            password,
            cancel,
            shutdown: self.supervisor.cancel.clone(),
            start,
        };
        let finished = self.client.0.clone();
        self.supervisor.spawn(async move {
            let job = attempt.start.job;
            attempt.run().await;
            let _ = finished.send(Internal::Extracted(job)).await;
        });
    }

    /// An attempt ended; the next may start.
    pub(super) fn extracted(&mut self, job: JobId) {
        if self
            .extractions
            .running
            .as_ref()
            .is_some_and(|(running, _)| *running == job)
        {
            self.extractions.running = None;
        }
    }

    /// Removes private folders that attempts left behind, such as after a
    /// crash while one was cancelled.
    pub(super) async fn clean_extractions(&mut self) {
        let Ok(stale) = self.with_store(|store| store.stale_extractions()).await else {
            return;
        };
        let instance = self.identity.instance;
        for (root, job, staging) in stale {
            let Some(root) = self.destinations.get(root) else {
                continue;
            };
            let (fs, store) = (Probes::new(self.supervisor.guard()), self.client.clone());
            self.supervisor.spawn(async move {
                let name = staging.clone();
                let removed = fs
                    .blocking(move || {
                        Root::open(&root.path, root.id, instance)?.remove_extraction(&name)
                    })
                    .await;
                if matches!(removed, Ok(Ok(()))) {
                    let _ = store
                        .call(move |store| store.forget_extraction_staging(job, &staging))
                        .await;
                }
            });
        }
    }

    /// Keeps `password` for `job`'s next attempt, from a request to unpack
    /// it.
    pub(super) fn give_password(
        &mut self,
        job: JobId,
        password: String,
    ) -> Result<(), ProtocolError> {
        if let Some(why) = self.extractions.unavailable() {
            return Err(why);
        }
        self.extractions
            .give_password(job, Secret::from_text(&password));
        Ok(())
    }
}

/// One attempt to unpack an archive, run as an engine task.
struct Attempt {
    store: StoreClient,
    fs: Probes,
    root: Option<Destination>,
    instance: wayfetch_protocol::InstanceId,
    extractor: Arc<dyn Extractor>,
    password: Option<Secret>,
    /// Cancels this attempt, or every engine task at shutdown.
    cancel: CancellationToken,
    /// Cancelled only at shutdown.
    shutdown: CancellationToken,
    start: ExtractionStart,
}

/// Why an attempt stopped before it ended.
enum Stop {
    /// It ended: record this.
    End(ExtractionEnd),
    /// It was cancelled, or the engine is stopping.
    Cancelled,
}

impl From<ExtractionEnd> for Stop {
    fn from(end: ExtractionEnd) -> Self {
        Self::End(end)
    }
}

/// What a person is told when a blocking step could not run.
fn stopped() -> ExtractionEnd {
    ExtractionEnd::Failed("Unpacking stopped unexpectedly.".into())
}

/// What a person is told about a filesystem failure.
fn folder_problem(e: &FsError) -> ExtractionEnd {
    ExtractionEnd::Failed(match e {
        FsError::PublishedMissing(_) => "The archive is no longer in its folder.".into(),
        FsError::PublishedChanged(_) => "The archive was changed outside Wayfetch.".into(),
        e if e.is_absent_root() => {
            "The download folder is unavailable; its drive may not be mounted.".into()
        }
        e => format!("The download folder could not be written: {e}."),
    })
}

/// What a person is told about an extractor failure.
fn extractor_problem(e: ExtractError) -> ExtractionEnd {
    match e {
        ExtractError::NeedsPassword => ExtractionEnd::NeedsPassword(None),
        ExtractError::WrongPassword => {
            ExtractionEnd::NeedsPassword(Some("The password is wrong.".into()))
        }
        ExtractError::MissingVolume => {
            ExtractionEnd::Failed("A part of the archive is missing.".into())
        }
        ExtractError::Unsupported(why) | ExtractError::Damaged(why) | ExtractError::Failed(why) => {
            ExtractionEnd::Failed(why)
        }
    }
}

impl Attempt {
    async fn run(self) {
        let job = self.start.job;
        let generation = self.start.generation;
        let staging = self.start.staging.clone();
        let (root, stop) = match self.open().await {
            Ok((root, output)) => {
                let stop = self.unpack(&root, &output).await;
                (Some(root), stop)
            }
            Err(stop) => (None, stop),
        };
        // At shutdown the attempt is left for the next start, which removes
        // its folder and tries again.
        if self.shutdown.is_cancelled() {
            return;
        }
        let complete = matches!(stop, Stop::End(ExtractionEnd::Complete(_)));
        if let (Some(root), false) = (root, complete) {
            let name = staging.clone();
            let _ = self
                .fs
                .blocking(move || root.remove_extraction(&name))
                .await;
        }
        let end = match stop {
            Stop::End(end) => end,
            // Recorded as cancelled already; this forgets the folder.
            Stop::Cancelled => ExtractionEnd::Failed("Cancelled.".into()),
        };
        let _ = self
            .store
            .call(move |store| store.finish_extraction(job, generation, &end, now()))
            .await;
    }

    /// Opens the archive's root and gives the attempt its empty private
    /// folder, removing an earlier attempt's.
    async fn open(&self) -> Result<(Arc<Root>, std::path::PathBuf), Stop> {
        let Some(root) = self.root.clone() else {
            return Err(ExtractionEnd::Failed(
                "The archive's folder is no longer a download folder.".into(),
            )
            .into());
        };
        let (instance, leftover, staging) = (
            self.instance,
            self.start.leftover.clone(),
            self.start.staging.clone(),
        );
        let opened = self
            .fs
            .blocking(move || -> Result<_, FsError> {
                let root = Root::open(&root.path, root.id, instance)?;
                if let Some(leftover) = leftover {
                    root.remove_extraction(&leftover)?;
                }
                let output = root.create_extraction(&staging)?;
                Ok((Arc::new(root), output))
            })
            .await
            .map_err(|_| Stop::End(stopped()))?;
        opened.map_err(|e| Stop::End(folder_problem(&e)))
    }

    /// Runs `work` on the root on a blocking thread.
    async fn on_root<T: Send + 'static>(
        &self,
        root: &Arc<Root>,
        work: impl FnOnce(&Root) -> T + Send + 'static,
    ) -> Result<T, Stop> {
        let root = root.clone();
        self.fs
            .blocking(move || work(&root))
            .await
            .map_err(|_| Stop::End(stopped()))
    }

    /// Lists, unpacks, checks, and publishes the archive.
    async fn unpack(&self, root: &Arc<Root>, output: &std::path::Path) -> Stop {
        match self.try_unpack(root, output).await {
            Ok(output) => ExtractionEnd::Complete(output).into(),
            Err(stop) => stop,
        }
    }

    async fn try_unpack(
        &self,
        root: &Arc<Root>,
        output: &std::path::Path,
    ) -> Result<crate::fs::RelativePath, Stop> {
        let file = self.start.archive.clone();
        let path = self
            .on_root(root, move |root| root.published_path(&file))
            .await?
            .map_err(|e| Stop::End(folder_problem(&e)))?;
        let archive = Archive {
            path: &path,
            layout: self.start.layout,
            password: self.password.as_ref(),
        };
        // A compressed tar lists only the tar inside; it is measured while
        // it is written instead.
        let declared = if self.start.layout == Layout::Direct {
            let entries = self
                .cancellable(self.extractor.list(archive))
                .await?
                .map_err(|e| Stop::End(extractor_problem(e)))?;
            if self.password.is_none() && entries.iter().any(|e| e.encrypted) {
                return Err(ExtractionEnd::NeedsPassword(None).into());
            }
            extract::check_listing(&entries).map_err(|why| Stop::End(ExtractionEnd::Failed(why)))?
        } else {
            None
        };
        let budget = extract::budget(declared, self.start.archive.size);
        let unpacking = self.extractor.extract(archive, output);
        tokio::pin!(unpacking);
        let mut watch = tokio::time::interval(WATCH_INTERVAL);
        loop {
            tokio::select! {
                () = self.cancel.cancelled() => return Err(Stop::Cancelled),
                done = &mut unpacking => {
                    done.map_err(|e| Stop::End(extractor_problem(e)))?;
                    break;
                }
                _ = watch.tick() => {
                    let staging = self.start.staging.clone();
                    let (tally, free) = self
                        .on_root(root, move |root| root.tally_extraction(&staging, budget))
                        .await?
                        .map_err(|e| Stop::End(folder_problem(&e)))?;
                    if tally.bytes > budget || tally.entries > extract::MAX_ENTRIES {
                        return Err(ExtractionEnd::Failed(
                            "The archive unpacks to far more than it should; it may be a \
                             decompression bomb."
                                .into(),
                        )
                        .into());
                    }
                    if free.is_some_and(|free| free < RESERVE_BYTES) {
                        return Err(ExtractionEnd::Failed(
                            "The download folder's drive is almost full.".into(),
                        )
                        .into());
                    }
                }
            }
        }
        let (staging, parent, stem) = (
            self.start.staging.clone(),
            self.start.archive.path.split_last().0.map(str::to_owned),
            self.start.stem.clone(),
        );
        self.on_root(root, move |root| {
            root.check_extraction(&staging, budget)?;
            root.publish_extraction(&staging, parent.as_deref(), &stem)
                .map_err(TreeError::from)
        })
        .await?
        .map_err(|e| match e {
            TreeError::Refused(why) => Stop::End(ExtractionEnd::Failed(why)),
            TreeError::Fs(e) => Stop::End(folder_problem(&e)),
        })
    }

    /// `work`, unless the attempt is cancelled first.
    async fn cancellable<T>(&self, work: impl Future<Output = T>) -> Result<T, Stop> {
        tokio::select! {
            () = self.cancel.cancelled() => Err(Stop::Cancelled),
            done = work => Ok(done),
        }
    }
}
