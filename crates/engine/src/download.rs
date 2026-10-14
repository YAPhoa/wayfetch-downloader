//! One job's work for a direct link: request, stream into owned staging,
//! verify, and publish, or reconcile what a previous run left (architecture
//! brief sections 7.5 to 7.7 and the first slice of section 14.3).
//!
//! A [`Download`] runs as an engine task. It never holds the store: every
//! state change is a generation-checked call the coordinator runs in turn,
//! so a stale worker (cancelled, or left over from before a restart) cannot
//! change the job. The stages it moves the job through:
//!
//! * A queued job is admitted as `resolving`. A direct link needs no
//!   provider resolution, so this is where the request is sent.
//! * A `recovering` job (active when the engine last stopped) first
//!   reconciles an interrupted publication, then adopts its staging file
//!   if the durable bytes can be continued.
//! * Bytes from several responses are combined only when the stored
//!   representation is trustworthy and the server reports it unchanged
//!   (rules 7.5.4 and 7.5.5); otherwise the job is queued again under a new
//!   generation and starts from zero (rule 7.5.2).
//! * `verifying` has no checksum to compare for a direct link; publication
//!   itself requires durable coverage of exactly the artifact's length.
//!
//! Provider leases, retries with backoff, and segmented transfers arrive
//! with the provider slice; until then a request that fails parks the job
//! as failed with the failure the transport classified.

use std::any::Any;
use std::sync::Arc;
use std::time::SystemTime;

use percent_encoding::percent_decode_str;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use url::Url;
use wayfetch_domain::failure::FailureCode;
use wayfetch_domain::id::JobId;
use wayfetch_domain::job::{DesiredState, JobStage};
use wayfetch_domain::name::DiagnosticText;
use wayfetch_domain::time::UtcTimestamp;
use wayfetch_domain::version::BootEpoch;
use wayfetch_protocol::InstanceId;
use wayfetch_protocol::query::FailureSummary;

use crate::config::{Limits, RootConfig, Timeouts};
use crate::fs::{FsError, PublicationCheck, RelativePath, Root, StagingFile, StagingRecord};
use crate::lifecycle::StoreClient;
use crate::store::{Admission, Partial, Store, StoreError};
use crate::transfer::lease::Representation;
use crate::transfer::range::{self, Plan};
use crate::transfer::transport::{OpenRequest, Response, Transport};
use crate::transfer::writer::{IntervalWriter, StreamEnd, WriteError};

/// Alternative names tried when a target name is taken.
const NAME_ATTEMPTS: u32 = 100;

/// Why a download stopped before completing its job.
#[derive(Debug)]
enum Stop {
    /// Leave the job as it is: the engine is stopping, the work is stale,
    /// or storage failed. Restart recovery takes it over.
    Abandon,
    /// The user paused or cancelled; the coordinator settles the job.
    Interrupted,
    /// Park the job with a failure.
    Park(JobStage, FailureCode, String),
    /// Leave the partial and queue the job again under a new generation,
    /// which starts from zero.
    Restart,
}

impl From<StoreError> for Stop {
    /// A stale generation means the work was abandoned; anything else is a
    /// storage fault that recovery retries after a restart.
    fn from(_: StoreError) -> Self {
        Stop::Abandon
    }
}

fn failed(code: FailureCode, message: impl ToString) -> Stop {
    Stop::Park(JobStage::Failed, code, message.to_string())
}

/// Parks on a filesystem error: blocked on an unavailable destination,
/// failed otherwise.
fn park_fs(err: &FsError) -> Stop {
    let code = err.failure_code();
    let stage = match code {
        FailureCode::IntegrityMismatch => JobStage::Failed,
        _ => JobStage::Blocked,
    };
    Stop::Park(stage, code, err.to_string())
}

/// Whether a filesystem error means the partial is gone or damaged, so the
/// job must start again rather than block.
fn partial_lost(err: &FsError) -> bool {
    err.failure_code() == FailureCode::IntegrityMismatch
}

fn now() -> UtcTimestamp {
    UtcTimestamp::from_system_time(SystemTime::now())
}

/// A partial the job continues instead of starting from zero.
struct Continued {
    file: StagingFile,
    partial: Partial,
}

/// Where the body goes.
struct Target {
    start: u64,
    end: Option<u64>,
}

/// One job's download, run as an engine task.
pub(crate) struct Download {
    pub(crate) store: StoreClient,
    pub(crate) transport: Arc<dyn Transport>,
    pub(crate) root: RootConfig,
    pub(crate) instance: InstanceId,
    pub(crate) boot_epoch: BootEpoch,
    pub(crate) limits: Limits,
    pub(crate) timeouts: Timeouts,
    /// Fires when the user pauses or cancels the job, or the engine stops.
    pub(crate) cancel: CancellationToken,
    /// Held by every blocking file operation (the state-directory lock).
    pub(crate) guard: Arc<dyn Any + Send + Sync>,
    pub(crate) job: Admission,
}

impl Download {
    /// Runs the job until it completes, parks, or stops.
    pub(crate) async fn run(mut self) {
        let stopped = match self.drive().await {
            Ok(()) | Err(Stop::Abandon | Stop::Interrupted) => return,
            Err(stop) => stop,
        };
        let (job, generation, from) = (self.job.job, self.job.generation, self.job.stage);
        // A stale generation is rejected here too, which is what a
        // cancellation that raced this failure needs.
        let _ = self
            .store
            .call(move |store| match stopped {
                Stop::Park(to, code, message) => store.record_stage(
                    job,
                    to,
                    generation,
                    Some(FailureSummary {
                        code,
                        retry_at: None,
                        message: Some(DiagnosticText::new(&message)),
                    }),
                    now(),
                ),
                // Publication cannot be undone into a restart.
                Stop::Restart if from == JobStage::Publishing => store.record_stage(
                    job,
                    JobStage::Failed,
                    generation,
                    Some(FailureSummary {
                        code: FailureCode::IntegrityMismatch,
                        retry_at: None,
                        message: Some(DiagnosticText::new("staging file lost during publication")),
                    }),
                    now(),
                ),
                _ => {
                    // Only a resolving, recovering, or verifying job may
                    // queue again; a transferring one steps back first.
                    if from == JobStage::Transferring {
                        store.advance_job(job, JobStage::Resolving, generation, None, now())?;
                    }
                    store.advance_job(job, JobStage::Queued, generation, None, now())
                }
            })
            .await;
    }

    async fn drive(&mut self) -> Result<(), Stop> {
        let url =
            Url::parse(&self.job.url).map_err(|e| failed(FailureCode::AdapterParseFailure, e))?;
        let root = Arc::new(self.open_root().await?);
        if self.job.stage == JobStage::Recovering {
            if self.reconcile_publication(&root).await? {
                return Ok(());
            }
            if self.job.desired != DesiredState::Running {
                return Err(Stop::Interrupted);
            }
        }
        let continued = match self.continue_partial(&root).await? {
            // Every byte is already durable, for example after a crash
            // between the last checkpoint and publication.
            Some(c) if c.partial.is_complete() => {
                let name = target_name(self.job.job, c.partial.suggested_name.as_deref(), &url);
                return self
                    .finish(&root, c.file, c.partial.durable_end, name)
                    .await;
            }
            other => other,
        };
        let (response, target) = self.request(&url, continued.as_ref()).await?;
        let suggested = match &continued {
            Some(c) => c.partial.suggested_name.clone(),
            None => response.filename.clone(),
        };
        self.advance(JobStage::Transferring).await?;
        let file = match continued {
            Some(c) => c.file,
            None => {
                let representation = response.representation.clone();
                self.create_staging(&root, representation, suggested.clone())
                    .await?
            }
        };
        let (size, file) = self.stream(file, response, &target).await?;
        let name = target_name(self.job.job, suggested.as_deref(), &url);
        self.finish(&root, file, size, name).await
    }

    /// Opens the download root, blocking the job if it is unavailable.
    async fn open_root(&self) -> Result<Root, Stop> {
        let (path, id, instance) = (self.root.path.clone(), self.root.id, self.instance);
        self.blocking(move || Root::open(&path, id, instance))
            .await?
            .map_err(|e| park_fs(&e))
    }

    /// Settles a publication a previous run left unfinished. Returns `true`
    /// if the job is now complete.
    async fn reconcile_publication(&mut self, root: &Arc<Root>) -> Result<bool, Stop> {
        let job = self.job.job;
        let Some(intent) = self
            .call(move |store| store.pending_publication(job))
            .await??
        else {
            return Ok(false);
        };
        let check = {
            let (root, intent) = (root.clone(), intent.clone());
            self.blocking(move || {
                root.reconcile_publication(&intent.staging, &intent.target, intent.size)
            })
            .await?
            .map_err(|e| park_fs(&e))?
        };
        let generation = self.job.generation;
        match check {
            PublicationCheck::Published => {}
            PublicationCheck::NotPublished => {
                let file = self
                    .reopen(root, intent.staging.clone(), intent.size)
                    .await?;
                let (r, target, size) = (root.clone(), intent.target.clone(), intent.size);
                match self
                    .blocking(move || r.publish(file, &target, size))
                    .await?
                {
                    Ok(()) => {}
                    Err(FsError::TargetExists(_)) => {
                        return self
                            .publish_elsewhere(root, &intent.target, intent.size)
                            .await;
                    }
                    Err(e) if partial_lost(&e) => {
                        self.call(move |s| s.abandon_publication(job, generation))
                            .await??;
                        return Err(Stop::Restart);
                    }
                    Err(e) => return Err(park_fs(&e)),
                }
            }
            PublicationCheck::Conflict => {
                return self
                    .publish_elsewhere(root, &intent.target, intent.size)
                    .await;
            }
            PublicationCheck::Lost | PublicationCheck::Damaged => {
                self.call(move |s| s.abandon_publication(job, generation))
                    .await??;
                return Err(Stop::Restart);
            }
        }
        self.call(move |s| s.complete_publication(job, generation, now()))
            .await??;
        Ok(true)
    }

    /// Another file took the intended name: release it, adopt the staging
    /// file of `size` bytes, and publish it under an alternative of the
    /// same name.
    async fn publish_elsewhere(
        &mut self,
        root: &Arc<Root>,
        taken: &RelativePath,
        size: u64,
    ) -> Result<bool, Stop> {
        let (job, generation, epoch) = (self.job.job, self.job.generation, self.boot_epoch);
        self.call(move |s| s.abandon_publication(job, generation))
            .await??;
        let adopted = self
            .call(move |s| s.adopt_staging(job, generation, epoch))
            .await??;
        let file = self.reopen(root, adopted, size).await?;
        self.advance(JobStage::Publishing).await?;
        self.publish(root, file, size, taken, 1).await?;
        Ok(true)
    }

    /// The job's staging file and durable bytes, if they can be continued:
    /// the generation's own file (a resumed pause), or after a restart the
    /// previous run's file, adopted by this generation. A partial that
    /// cannot be continued queues the job again.
    async fn continue_partial(&mut self, root: &Arc<Root>) -> Result<Option<Continued>, Stop> {
        let (job, generation, epoch) = (self.job.job, self.job.generation, self.boot_epoch);
        let Some(partial) = self.call(move |s| s.latest_staging(job)).await?? else {
            return Ok(None);
        };
        let own = partial.record.generation == generation;
        if !own && self.job.stage != JobStage::Recovering {
            // An older generation's file was abandoned by a cancel or a
            // restart from zero; it is left for cleanup.
            return Ok(None);
        }
        let trusted = partial
            .representation
            .as_ref()
            .is_some_and(|r| r.is_trustworthy() || r.length == Some(partial.durable_end));
        if partial.durable_end == 0 || !partial.contiguous || !trusted {
            return Err(Stop::Restart);
        }
        let file = self
            .reopen(root, partial.record.clone(), partial.durable_end)
            .await?;
        if own {
            return Ok(Some(Continued { file, partial }));
        }
        drop(file);
        let adopted = self
            .call(move |s| s.adopt_staging(job, generation, epoch))
            .await??;
        let file = self
            .reopen(root, adopted.clone(), partial.durable_end)
            .await?;
        Ok(Some(Continued {
            file,
            partial: Partial {
                record: adopted,
                ..partial
            },
        }))
    }

    /// Reopens a recorded staging file holding `durable_end` bytes. A
    /// missing, replaced, or shrunk file restarts the job.
    async fn reopen(
        &self,
        root: &Arc<Root>,
        record: StagingRecord,
        durable_end: u64,
    ) -> Result<StagingFile, Stop> {
        let root = root.clone();
        match self
            .blocking(move || root.reopen_staging(&record, durable_end))
            .await?
        {
            Ok(file) => Ok(file),
            Err(e) if partial_lost(&e) => Err(Stop::Restart),
            Err(e) => Err(park_fs(&e)),
        }
    }

    /// Sends the request, from zero or after a continued partial's durable
    /// bytes, and checks the response before any byte is written.
    async fn request(
        &self,
        url: &Url,
        continued: Option<&Continued>,
    ) -> Result<(Response, Target), Stop> {
        let stored = continued.and_then(|c| c.partial.representation.clone());
        let requested = continued.map(|c| (c.partial.durable_end, None));
        let request = OpenRequest {
            url: url.clone(),
            range: requested,
            validators: stored.clone(),
            connect_timeout: self.timeouts.connect,
            read_buffer: self.limits.read_buffer_bytes.get(),
        };
        let deadline = self.timeouts.response;
        let response = tokio::select! {
            biased;
            () = self.cancel.cancelled() => return Err(Stop::Interrupted),
            opened = tokio::time::timeout(deadline, self.transport.open(&request)) => match opened {
                Err(_) => {
                    return Err(failed(
                        FailureCode::RequestTimeout,
                        format!("no response headers within {deadline:?}"),
                    ));
                }
                Ok(Err(e)) => return Err(failed(e.code, e.message)),
                Ok(Ok(response)) => response,
            },
        };
        let known = stored.as_ref().and_then(|r| r.length);
        let plan = range::plan(requested, response.framing, known)
            .map_err(|e| failed(FailureCode::IntegrityMismatch, e))?;
        let Plan::Write {
            start,
            end,
            complete,
        } = plan
        else {
            // The server ignored the range, or reports nothing left to send
            // for a partial that is not known to be complete.
            return Err(Stop::Restart);
        };
        if stored.is_some_and(|stored| stored != response.representation) {
            return Err(Stop::Restart);
        }
        // One sequential stream must reach the end; requesting the rest of
        // a shorter answer is segmented transfer, which comes later.
        if let (Some(end), Some(complete)) = (end, complete)
            && end != complete
        {
            return Err(failed(
                FailureCode::IntegrityMismatch,
                format!("response ends at byte {end} of {complete}"),
            ));
        }
        Ok((response, Target { start, end }))
    }

    /// Creates, registers, and describes a new staging file.
    async fn create_staging(
        &self,
        root: &Arc<Root>,
        representation: Representation,
        suggested: Option<String>,
    ) -> Result<StagingFile, Stop> {
        let (job, generation, epoch) = (self.job.job, self.job.generation, self.boot_epoch);
        let r = root.clone();
        let file = self
            .blocking(move || r.create_staging(job, generation))
            .await?
            .map_err(|e| park_fs(&e))?;
        let record = file.record().clone();
        self.call(move |s| {
            s.register_staging(&record, epoch, now())?;
            s.record_response(
                job,
                generation,
                &representation,
                suggested.as_deref(),
                now(),
            )
        })
        .await??;
        Ok(file)
    }

    /// Streams the body into `file`, committing checkpoints as it goes.
    /// Returns the artifact's size and the file.
    async fn stream(
        &self,
        file: StagingFile,
        mut response: Response,
        target: &Target,
    ) -> Result<(u64, StagingFile), Stop> {
        let mut writer = IntervalWriter::new(Arc::new(file), target.start, target.end)
            .holding(self.guard.clone());
        let (checkpoints, mut certified) = mpsc::channel(1);
        let streaming = async {
            let end = writer
                .stream(
                    response.body.as_mut(),
                    &self.cancel,
                    &checkpoints,
                    self.limits.checkpoint_bytes.get(),
                    self.timeouts.idle_read,
                )
                .await;
            drop(checkpoints);
            end
        };
        let committing = async {
            while let Some(synced) = certified.recv().await {
                self.call(move |s| s.commit_checkpoint(synced)).await??;
            }
            Ok::<(), Stop>(())
        };
        let (end, committed) = tokio::join!(streaming, committing);
        committed?;
        match end {
            Ok(StreamEnd::Finished) => {
                let size = writer.position();
                // Every write has finished, so the writer held the last
                // reference.
                let file = Arc::try_unwrap(writer.into_file()).map_err(|_| Stop::Abandon)?;
                Ok((size, file))
            }
            Ok(StreamEnd::Cancelled) => {
                // A pause keeps the generation, so its last bytes count. A
                // cancel's checkpoint is stale and rejected.
                if let Ok(Some(synced)) = writer.checkpoint().await {
                    let _ = self.call(move |s| s.commit_checkpoint(synced)).await;
                }
                Err(Stop::Interrupted)
            }
            Err(WriteError::Stalled(after)) => Err(failed(
                FailureCode::RequestTimeout,
                format!("no data for {after:?}"),
            )),
            Err(WriteError::Source(e)) => Err(failed(FailureCode::RequestTimeout, e)),
            Err(e @ (WriteError::Overflow { .. } | WriteError::Truncated { .. })) => {
                Err(failed(FailureCode::IntegrityMismatch, e))
            }
            Err(e @ WriteError::File(_)) => Err(Stop::Park(
                JobStage::Blocked,
                FailureCode::DestinationUnavailable,
                e.to_string(),
            )),
            // The commit loop stopped first and said why.
            Err(WriteError::CheckpointsClosed) => Err(Stop::Abandon),
        }
    }

    /// Verifies and publishes a finished staging file of `size` bytes.
    async fn finish(
        &mut self,
        root: &Arc<Root>,
        file: StagingFile,
        size: u64,
        name: RelativePath,
    ) -> Result<(), Stop> {
        if self.job.stage == JobStage::Resolving {
            self.advance(JobStage::Transferring).await?;
        }
        self.advance(JobStage::Verifying).await?;
        self.advance(JobStage::Publishing).await?;
        self.publish(root, file, size, &name, 0).await
    }

    /// Publishes `file` under `name`, or its first free numbered
    /// alternative from `first` on, and completes the job.
    async fn publish(
        &mut self,
        root: &Arc<Root>,
        file: StagingFile,
        size: u64,
        name: &RelativePath,
        first: u32,
    ) -> Result<(), Stop> {
        let (job, generation) = (self.job.job, self.job.generation);
        let record = file.record().clone();
        let mut file = Some(file);
        for n in first..first + NAME_ATTEMPTS {
            let Ok(target) = name.numbered(n) else {
                break;
            };
            let reserved = target.clone();
            match self
                .call(move |s| s.begin_publication(job, generation, &reserved, size, now()))
                .await?
            {
                Ok(()) => {}
                Err(StoreError::TargetReserved(_)) => continue,
                Err(e) => return Err(e.into()),
            }
            let staging = match file.take() {
                Some(f) => f,
                None => self.reopen(root, record.clone(), size).await?,
            };
            let (r, t) = (root.clone(), target.clone());
            match self.blocking(move || r.publish(staging, &t, size)).await? {
                Ok(()) => {
                    self.call(move |s| s.complete_publication(job, generation, now()))
                        .await??;
                    return Ok(());
                }
                Err(FsError::TargetExists(_)) => {
                    self.call(move |s| s.abandon_publication(job, generation))
                        .await??;
                }
                Err(e) => return Err(park_fs(&e)),
            }
        }
        Err(Stop::Park(
            JobStage::Blocked,
            FailureCode::DestinationUnavailable,
            format!("no free name like {name} after {NAME_ATTEMPTS} attempts"),
        ))
    }

    /// Moves the job to `to` under its generation.
    async fn advance(&mut self, to: JobStage) -> Result<(), Stop> {
        let (job, generation) = (self.job.job, self.job.generation);
        self.call(move |s| s.advance_job(job, to, generation, None, now()))
            .await??;
        self.job.stage = to;
        Ok(())
    }

    /// Runs store work on the coordinator.
    async fn call<R: Send + 'static>(
        &self,
        work: impl FnOnce(&mut Store) -> R + Send + 'static,
    ) -> Result<R, Stop> {
        self.store.call(work).await.ok_or(Stop::Abandon)
    }

    /// Runs file work on the blocking pool, holding the state-directory
    /// lock until it ends.
    async fn blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, Stop> {
        let guard = self.guard.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            work()
        })
        .await
        .map_err(|_| Stop::Abandon)
    }
}

/// The artifact's name: the server's suggestion, else the URL's last path
/// segment, else a name derived from the job. Only a single valid path
/// component is accepted, so neither can choose a directory.
fn target_name(job: JobId, suggested: Option<&str>, url: &Url) -> RelativePath {
    let from_url = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .and_then(|segment| percent_decode_str(segment).decode_utf8().ok())
        .map(|name| name.into_owned());
    suggested
        .map(str::to_owned)
        .into_iter()
        .chain(from_url)
        .filter(|name| !name.contains('/'))
        .find_map(|name| RelativePath::new(&name).ok())
        .unwrap_or_else(|| {
            RelativePath::new(&format!("download-{}", job.get())).expect("a valid file name")
        })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use wayfetch_domain::id::RootId;
    use wayfetch_protocol::command::{InputUrl, SubmitItems};
    use wayfetch_protocol::query::JobSummary;
    use wayfetch_protocol::{Command, CommandEnvelope, IdempotencyKey, Query, QueryResult};

    use super::*;
    use crate::transfer::writer::BoxFuture;

    const URL: &str = "https://files.example/a.bin";

    /// Recovery of a publication needs no request.
    struct NoRequests;

    impl Transport for NoRequests {
        fn open<'a>(
            &'a self,
            _: &'a OpenRequest,
        ) -> BoxFuture<'a, Result<Response, crate::transfer::transport::TransportError>> {
            panic!("no request expected")
        }
    }

    struct Crashed {
        _state: tempfile::TempDir,
        root_dir: tempfile::TempDir,
        store: StoreClient,
        instance: InstanceId,
        job: JobId,
        download: Option<Download>,
        staging: StagingRecord,
    }

    impl Crashed {
        /// Runs the recovering job's download, as the next start does.
        async fn recover(&mut self) -> JobSummary {
            self.download.take().unwrap().run().await;
            let (job, instance) = (self.job, self.instance);
            let result = self
                .store
                .call(move |s| s.query(&Query::InspectJob { job }, instance))
                .await
                .unwrap()
                .unwrap();
            match result {
                QueryResult::Job { job, .. } => job,
                other => panic!("{other:?}"),
            }
        }

        fn read(&self, name: &str) -> Vec<u8> {
            std::fs::read(self.root_dir.path().join(name)).unwrap()
        }

        fn staging_exists(&self) -> bool {
            crate::fs::staging_path(self.root_dir.path(), &self.staging.name).exists()
        }
    }

    /// A job whose engine stopped after recording the intent to publish
    /// `hello` as `a.bin`; `crash` changes the files as the crash left
    /// them. Returns the recovering job as the next start sees it (T24).
    fn crashed_publishing(crash: impl FnOnce(&Root, &Path, &StagingRecord)) -> Crashed {
        let state = tempfile::tempdir().unwrap();
        let root_dir = tempfile::tempdir().unwrap();
        let (root_id, instance) = (RootId::from_u64(1).unwrap(), InstanceId::generate());
        Root::initialize(root_dir.path(), root_id, instance).unwrap();
        let root = Root::open(root_dir.path(), root_id, instance).unwrap();
        let mut store = Store::open(state.path()).unwrap();
        let submit = CommandEnvelope {
            idempotency_key: IdempotencyKey::generate(),
            target: instance,
            command: Command::Submit {
                items: SubmitItems::try_from(vec![InputUrl::parse(URL).unwrap()]).unwrap(),
                package: None,
            },
        };
        store.execute(&submit, instance, now()).unwrap();
        let job = store.stage_pending(1, now()).unwrap()[0];
        let generation = store.job_state(job).unwrap().generation();
        let advance = |store: &mut Store, stage| {
            store
                .advance_job(job, stage, generation, None, now())
                .unwrap();
        };
        advance(&mut store, JobStage::Resolving);
        advance(&mut store, JobStage::Transferring);
        let file = root.create_staging(job, generation).unwrap();
        let staging = file.record().clone();
        store
            .register_staging(&staging, BootEpoch::INITIAL, now())
            .unwrap();
        let representation = Representation {
            length: Some(5),
            strong_etag: Some("\"v1\"".into()),
            last_modified: None,
        };
        store
            .record_response(job, generation, &representation, None, now())
            .unwrap();
        file.write_all_at(b"hello", 0).unwrap();
        #[expect(clippy::single_range_in_vec_init, reason = "a list of one byte range")]
        let synced = file.sync_ranges(vec![0..5]).unwrap();
        store.commit_checkpoint(synced).unwrap();
        advance(&mut store, JobStage::Verifying);
        advance(&mut store, JobStage::Publishing);
        let target = RelativePath::new("a.bin").unwrap();
        store
            .begin_publication(job, generation, &target, 5, now())
            .unwrap();
        drop(file);
        crash(&root, root_dir.path(), &staging);

        drop(store);
        let mut store = Store::open(state.path()).unwrap();
        store.recover_after_restart(now()).unwrap();
        let recovering = store.job_state(job).unwrap();
        assert_eq!(recovering.stage(), JobStage::Recovering);
        let client = StoreClient::serve(store);
        let download = Download {
            store: client.clone(),
            transport: Arc::new(NoRequests),
            root: RootConfig {
                id: root_id,
                path: root_dir.path().to_owned(),
            },
            instance,
            boot_epoch: BootEpoch::INITIAL.next().unwrap(),
            limits: Limits::DEFAULT,
            timeouts: Timeouts::DEFAULT,
            cancel: CancellationToken::new(),
            guard: Arc::new(()),
            job: Admission {
                job,
                stage: JobStage::Recovering,
                generation: recovering.generation(),
                desired: DesiredState::Running,
                url: URL.into(),
            },
        };
        Crashed {
            _state: state,
            root_dir,
            store: client,
            instance,
            job,
            download: Some(download),
            staging,
        }
    }

    fn target() -> RelativePath {
        RelativePath::new("a.bin").unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn recovery_finishes_a_publication_interrupted_before_or_after_the_link() {
        type Crash = fn(&Root, &Path, &StagingRecord);
        let cases: [(&str, Crash); 2] = [
            ("before the link", |_, _, _| {}),
            ("after the link", |root, _, record| {
                let file = root.reopen_staging(record, 5).unwrap();
                root.publish(file, &target(), 5).unwrap();
            }),
        ];
        for (name, crash) in cases {
            let mut crashed = crashed_publishing(crash);
            let done = crashed.recover().await;
            assert_eq!(done.stage, JobStage::Complete, "{name}");
            assert_eq!(done.artifact.unwrap().path, "a.bin", "{name}");
            assert_eq!(crashed.read("a.bin"), b"hello", "{name}");
            assert!(!crashed.staging_exists(), "{name}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn recovery_never_overwrites_a_file_that_took_the_name() {
        let mut crashed = crashed_publishing(|_, dir, _| {
            std::fs::write(dir.join("a.bin"), b"theirs").unwrap();
        });
        let done = crashed.recover().await;
        assert_eq!(done.stage, JobStage::Complete);
        assert_eq!(done.artifact.unwrap().path, "a (1).bin");
        assert_eq!(crashed.read("a.bin"), b"theirs");
        assert_eq!(crashed.read("a (1).bin"), b"hello");
        assert!(!crashed.staging_exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn recovery_restarts_a_job_whose_staged_file_is_lost_or_damaged() {
        type Crash = fn(&Root, &Path, &StagingRecord);
        let cases: [(&str, Crash); 2] = [
            ("lost", |_, dir, record| {
                std::fs::remove_file(crate::fs::staging_path(dir, &record.name)).unwrap();
            }),
            ("truncated", |_, dir, record| {
                let path = crate::fs::staging_path(dir, &record.name);
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(path)
                    .unwrap()
                    .set_len(2)
                    .unwrap();
            }),
        ];
        for (name, crash) in cases {
            let mut crashed = crashed_publishing(crash);
            let before = crashed.download.as_ref().unwrap().job.generation;
            let done = crashed.recover().await;
            // Queued again under a newer generation, which starts from zero.
            assert_eq!(done.stage, JobStage::Queued, "{name}");
            assert!(done.generation > before, "{name}");
            assert!(done.artifact.is_none(), "{name}");
            assert!(!crashed.root_dir.path().join("a.bin").exists(), "{name}");
        }
    }

    #[test]
    fn names_come_from_the_server_the_url_or_the_job() {
        let job = JobId::from_u64(7).unwrap();
        let url = Url::parse("https://example.test/files/My%20Game.part1.rar?x=1").unwrap();
        assert_eq!(target_name(job, None, &url).as_str(), "My Game.part1.rar");
        assert_eq!(
            target_name(job, Some("Server Name.zip"), &url).as_str(),
            "Server Name.zip"
        );
        // A suggestion that names a directory or an invalid name is ignored.
        for bad in ["../evil.zip", "dir/evil.zip", "CON", ".wayfetch"] {
            assert_eq!(
                target_name(job, Some(bad), &url).as_str(),
                "My Game.part1.rar",
                "{bad}"
            );
        }
        for bare in ["https://example.test/", "https://example.test/a/%2E%2E"] {
            let url = Url::parse(bare).unwrap();
            assert_eq!(
                target_name(job, None, &url).as_str(),
                "download-7",
                "{bare}"
            );
        }
        let encoded = Url::parse("https://example.test/a%2Fb.zip").unwrap();
        assert_eq!(target_name(job, None, &encoded).as_str(), "download-7");
    }
}
