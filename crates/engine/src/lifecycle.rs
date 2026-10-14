//! Engine startup, readiness, handles, scheduling, and bounded shutdown
//! (architecture brief sections 4.6 and 10, ADR-31 and ADR-34).
//!
//! * [`EngineBuilder`] starts an engine on a runtime the host supplies. The
//!   engine never creates a runtime, installs signal handlers, opens a
//!   listener, or exits the process.
//! * [`EngineOwner`] is held by the application host, not by a window. It
//!   owns the state-directory lock and every engine task, and is the only
//!   way to shut down.
//! * [`EngineHandle`] is a cloneable, bounded request facade for UIs and
//!   adapters. Dropping handles never stops the engine; a handle that
//!   outlives the engine gets `ShuttingDown` errors.
//!
//! Startup opens the store, retires work authorized by the previous run
//! (moving active jobs to recovery), stages submitted inputs, and prunes
//! history past retention before the engine reports ready. Before admitting
//! work, and after each download ends, the coordinator removes staging
//! files no job generation can reopen (see the cleanup module).
//!
//! The coordinator serializes every state change. It admits recovering,
//! then queued jobs while fewer than `limits.active_files` downloads run,
//! reading only as many from the database as it has free slots; each runs
//! as a download task whose store calls come back to the coordinator.
//! A pause or cancel stops the job's task, and the coordinator settles the
//! job once it has. After each change it broadcasts the new durable events
//! to subscribers through a ring of `limits.event_buffer` events.
//!
//! Shutdown stops admitting requests, cancels engine tasks, and waits up to a
//! deadline. Every engine task holds a reference to the state-directory lock,
//! so a task still running after the deadline keeps the directory locked and
//! the report says so; another engine cannot take over while old work may
//! still write.

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

use tokio::runtime::Handle;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use wayfetch_domain::id::JobId;
use wayfetch_domain::job::{DesiredState, JobStage};
use wayfetch_domain::time::UtcTimestamp;
use wayfetch_protocol::command::MAX_SUBMIT_ITEMS;
use wayfetch_protocol::event::{
    DurableEvent, EventCursor, ResyncReason, Retained, SubscriptionStart,
};
use wayfetch_protocol::{
    Command, CommandEnvelope, CommandResult, ErrorCode, Hello, InstanceId, Permission,
    ProtocolError, ProtocolVersion, Query, QueryResult, Subscribe, TransportLimits,
};

use crate::cleanup;
use crate::config::{ConfigError, EngineConfig};
use crate::download::Download;
use crate::fs::Root;
use crate::state_dir::{Identity, StateDir, StateDirError};
use crate::store::{Admission, Store, StoreError, StoreInfo};
use crate::subscription::Subscription;
use crate::transfer::transport::Transport;

/// How often history past retention is pruned while running.
const PRUNE_INTERVAL: Duration = Duration::from_secs(60);

/// Engine lifecycle phase, observable through [`EngineHandle::readiness`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Readiness {
    /// Starting; requests are not yet admitted.
    Starting,
    /// Admitting requests.
    Ready,
    /// Shutdown requested; new requests are refused.
    ShuttingDown,
    /// All engine tasks finished.
    Stopped,
    /// An engine task failed. The remaining work was cancelled and requests
    /// are refused; the host should shut the engine down and report it.
    Failed,
}

/// Why an engine could not start.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// The configuration is invalid. Nothing was touched on disk.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// The state directory is locked, missing, or unreadable.
    #[error(transparent)]
    StateDir(#[from] StateDirError),
    /// The database could not be opened, migrated, or recovered.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The supplied runtime is shutting down.
    #[error("runtime unavailable: {0}")]
    Runtime(#[from] tokio::task::JoinError),
}

/// Builds and starts an engine.
pub struct EngineBuilder {
    config: EngineConfig,
    runtime: Handle,
    transport: Option<Arc<dyn Transport>>,
}

impl std::fmt::Debug for EngineBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineBuilder")
            .field("config", &self.config)
            .field("transport", &self.transport.is_some())
            .finish_non_exhaustive()
    }
}

impl EngineBuilder {
    /// An engine that will run on `runtime`, which the host owns.
    pub fn new(config: EngineConfig, runtime: Handle) -> Self {
        Self {
            config,
            runtime,
            transport: None,
        }
    }

    /// The transport downloads use. Without one, jobs stay queued.
    pub fn transport(mut self, transport: Arc<dyn Transport>) -> Self {
        self.transport = Some(transport);
        self
    }

    /// Validates the configuration, takes ownership of the state directory,
    /// and starts the engine's tasks. Fails if another engine owns the
    /// directory.
    pub async fn start(self) -> Result<EngineOwner, StartError> {
        self.config.validate()?;
        let path = self.config.state_dir.clone();
        let limits = self.config.limits.clone();
        let (state_dir, store, head) = self
            .runtime
            .spawn_blocking(move || -> Result<_, StartError> {
                let state_dir = StateDir::open(&path)?;
                let mut store = Store::open(state_dir.path())?;
                store.recover_after_restart(now())?;
                stage_all(&mut store)?;
                prune(&mut store, &limits)?;
                let head = store.event_head()?;
                Ok((state_dir, store, head))
            })
            .await??;
        let identity = state_dir.identity();
        let store_info = store.info().clone();
        let (readiness_tx, readiness_rx) = watch::channel(Readiness::Starting);
        let (requests, inbox) = mpsc::channel(self.config.request_queue.get());
        // Each download waits for its one store call, so this never fills
        // beyond the number of downloads plus their completions.
        let (internal_tx, internal) = mpsc::channel(self.config.limits.active_files.get() * 2);
        let (events, _) = broadcast::channel(self.config.limits.event_buffer.get());
        let supervisor = Supervisor {
            runtime: self.runtime,
            cancel: CancellationToken::new(),
            tasks: TaskTracker::new(),
            failed_tasks: Arc::default(),
            readiness: readiness_tx,
            lock: Some(Arc::new(state_dir)),
        };
        let owner = EngineOwner {
            handle: EngineHandle {
                requests,
                readiness: readiness_rx,
                instance: identity.instance,
            },
            identity,
            store_info,
            shutdown_timeout: self.config.shutdown_timeout,
            supervisor: supervisor.clone(),
        };
        let coordinator = Coordinator {
            identity,
            inbox,
            internal,
            client: StoreClient(internal_tx),
            store: Some(store),
            supervisor,
            config: self.config,
            transport: self.transport,
            running: HashMap::new(),
            held: HashSet::new(),
            events,
            published: head,
            pruned_at: Instant::now(),
        };
        owner.supervisor.spawn(coordinator.run());
        // Unless a task already failed.
        owner.supervisor.readiness.send_if_modified(|r| {
            let starting = *r == Readiness::Starting;
            if starting {
                *r = Readiness::Ready;
            }
            starting
        });
        Ok(owner)
    }
}

/// Options for [`EngineOwner::shutdown`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ShutdownRequest {
    /// Time allowed for engine tasks to finish; the configured default when
    /// absent.
    pub timeout: Option<Duration>,
}

/// What a shutdown achieved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShutdownReport {
    /// Every engine task finished within the deadline and none failed.
    pub clean: bool,
    /// Engine tasks still running at the deadline.
    pub unfinished_tasks: usize,
    /// Engine tasks that panicked during the engine's lifetime.
    pub failed_tasks: usize,
    /// Whether the state-directory lock was released. False while any
    /// unfinished task still holds it.
    pub state_dir_released: bool,
    /// Time the shutdown took.
    pub elapsed: Duration,
}

/// Runs and tracks engine tasks.
#[derive(Debug, Clone)]
struct Supervisor {
    runtime: Handle,
    cancel: CancellationToken,
    tasks: TaskTracker,
    failed_tasks: Arc<AtomicUsize>,
    readiness: watch::Sender<Readiness>,
    /// The state-directory lock, held by every engine task.
    lock: Option<Arc<StateDir>>,
}

impl Supervisor {
    /// Spawns an engine task. The task keeps the state directory locked
    /// until it ends, and must stop promptly once the engine's cancellation
    /// token fires. Engine tasks are critical: a panic is counted, marks the
    /// engine [`Readiness::Failed`], and cancels the remaining work.
    fn spawn<F>(&self, task: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let guard = self.lock.clone();
        let work = self.runtime.spawn(async move {
            let _guard = guard;
            task.await;
        });
        let failed = Arc::clone(&self.failed_tasks);
        let readiness = self.readiness.clone();
        let cancel = self.cancel.clone();
        self.tasks.spawn_on(
            async move {
                if work.await.is_err() {
                    failed.fetch_add(1, Ordering::Relaxed);
                    readiness.send_if_modified(|r| {
                        let running = matches!(*r, Readiness::Starting | Readiness::Ready);
                        if running {
                            *r = Readiness::Failed;
                        }
                        running
                    });
                    cancel.cancel();
                }
            },
            &self.runtime,
        );
    }

    /// What blocking work holds so the lock outlives it.
    fn guard(&self) -> Arc<dyn Any + Send + Sync> {
        match &self.lock {
            Some(lock) => lock.clone(),
            None => Arc::new(()),
        }
    }
}

/// The running engine. Held by the application host.
#[derive(Debug)]
pub struct EngineOwner {
    handle: EngineHandle,
    identity: Identity,
    store_info: StoreInfo,
    shutdown_timeout: Duration,
    supervisor: Supervisor,
}

impl EngineOwner {
    /// A new handle for a UI, adapter, or test.
    pub fn handle(&self) -> EngineHandle {
        self.handle.clone()
    }

    /// The instance identity and this start's boot epoch.
    pub fn identity(&self) -> Identity {
        self.identity
    }

    /// The database settings in effect, for startup reporting.
    pub fn store_info(&self) -> &StoreInfo {
        &self.store_info
    }

    #[cfg(test)]
    pub(crate) fn spawn<F>(&self, task: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.supervisor.spawn(task);
    }

    /// Stops admitting requests, cancels engine tasks, and waits for them up
    /// to the deadline. Awaiting this is the only durability guarantee;
    /// dropping the owner cancels tasks without waiting.
    pub async fn shutdown(mut self, request: ShutdownRequest) -> ShutdownReport {
        let started = Instant::now();
        let timeout = request.timeout.unwrap_or(self.shutdown_timeout);
        let supervisor = &mut self.supervisor;
        let failed = *supervisor.readiness.borrow() == Readiness::Failed;
        if !failed {
            supervisor.readiness.send_replace(Readiness::ShuttingDown);
        }
        supervisor.cancel.cancel();
        supervisor.tasks.close();
        let finished = tokio::time::timeout(timeout, supervisor.tasks.wait())
            .await
            .is_ok();
        let state_dir_released = supervisor
            .lock
            .take()
            .is_some_and(|dir| Arc::try_unwrap(dir).is_ok());
        // A failed engine stays failed, so the host can still see why.
        if finished && *supervisor.readiness.borrow() != Readiness::Failed {
            supervisor.readiness.send_replace(Readiness::Stopped);
        }
        let failed_tasks = supervisor.failed_tasks.load(Ordering::Relaxed);
        ShutdownReport {
            clean: finished && failed_tasks == 0,
            unfinished_tasks: supervisor.tasks.len(),
            failed_tasks,
            state_dir_released,
            elapsed: started.elapsed(),
        }
    }
}

impl Drop for EngineOwner {
    fn drop(&mut self) {
        // Best effort only: a destructor cannot await durability.
        self.supervisor.readiness.send_if_modified(|r| {
            let changed = *r == Readiness::Ready || *r == Readiness::Starting;
            if changed {
                *r = Readiness::ShuttingDown;
            }
            changed
        });
        self.supervisor.cancel.cancel();
    }
}

enum Request {
    Hello(oneshot::Sender<Result<Hello, ProtocolError>>),
    Execute(
        Box<CommandEnvelope>,
        oneshot::Sender<Result<CommandResult, ProtocolError>>,
    ),
    Query(Query, oneshot::Sender<Result<QueryResult, ProtocolError>>),
    Subscribe(
        Subscribe,
        oneshot::Sender<Result<Subscription, ProtocolError>>,
    ),
    /// Runs arbitrary store work, for fault-injection tests.
    #[cfg(test)]
    Run(Box<dyn FnOnce(&mut Store) + Send>, oneshot::Sender<()>),
}

/// Cloneable, bounded request facade. Carries no database or filesystem
/// handle.
#[derive(Debug, Clone)]
pub struct EngineHandle {
    requests: mpsc::Sender<Request>,
    readiness: watch::Receiver<Readiness>,
    instance: InstanceId,
}

impl EngineHandle {
    /// The instance this handle talks to.
    pub fn instance(&self) -> InstanceId {
        self.instance
    }

    /// The engine's current lifecycle phase.
    pub fn readiness(&self) -> Readiness {
        *self.readiness.borrow()
    }

    /// Waits until the engine admits requests. Returns `false` if it is
    /// shutting down instead.
    pub async fn wait_until_ready(&self) -> bool {
        let mut rx = self.readiness.clone();
        match rx.wait_for(|r| *r != Readiness::Starting).await {
            Ok(r) => *r == Readiness::Ready,
            Err(_) => false,
        }
    }

    /// The connection handshake.
    pub async fn hello(&self) -> Result<Hello, ProtocolError> {
        let (reply, response) = oneshot::channel();
        self.send(Request::Hello(reply))?;
        response.await.map_err(|_| shutting_down())?
    }

    /// Runs a command durably. Success is returned only after the command
    /// committed; a retry with the same idempotency key replays the result
    /// while its record is retained (`limits.command_retention`). `granted`
    /// is what the host authenticated the caller for.
    pub async fn execute(
        &self,
        envelope: CommandEnvelope,
        granted: &[Permission],
    ) -> Result<CommandResult, ProtocolError> {
        authorize(envelope.command.required_permission(), granted)?;
        let (reply, response) = oneshot::channel();
        self.send(Request::Execute(Box::new(envelope), reply))?;
        response.await.map_err(|_| shutting_down())?
    }

    /// Answers a read query.
    pub async fn query(
        &self,
        query: Query,
        granted: &[Permission],
    ) -> Result<QueryResult, ProtocolError> {
        authorize(query.required_permission(), granted)?;
        let (reply, response) = oneshot::channel();
        self.send(Request::Query(query, reply))?;
        response.await.map_err(|_| shutting_down())?
    }

    /// Subscribes to durable events after `request.after`. A cursor from
    /// another instance, ahead of the head, pruned, or more than
    /// `limits.event_buffer` events behind yields a stream that only says
    /// to resynchronize.
    pub async fn subscribe(
        &self,
        request: Subscribe,
        granted: &[Permission],
    ) -> Result<Subscription, ProtocolError> {
        authorize(Permission::Read, granted)?;
        let (reply, response) = oneshot::channel();
        self.send(Request::Subscribe(request, reply))?;
        response.await.map_err(|_| shutting_down())?
    }

    fn send(&self, request: Request) -> Result<(), ProtocolError> {
        if self.readiness() != Readiness::Ready {
            return Err(shutting_down());
        }
        self.requests.try_send(request).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => {
                ProtocolError::new(ErrorCode::Overloaded, "engine request queue is full")
            }
            mpsc::error::TrySendError::Closed(_) => shutting_down(),
        })
    }
}

fn shutting_down() -> ProtocolError {
    ProtocolError::new(ErrorCode::ShuttingDown, "engine is not running")
}

fn authorize(required: Permission, granted: &[Permission]) -> Result<(), ProtocolError> {
    if granted.contains(&required) {
        Ok(())
    } else {
        Err(ProtocolError::new(
            ErrorCode::PermissionDenied,
            &format!("requires {required} permission"),
        ))
    }
}

fn now() -> UtcTimestamp {
    UtcTimestamp::from_system_time(SystemTime::now())
}

/// Stages every submitted input that has no job yet.
fn stage_all(store: &mut Store) -> Result<(), StoreError> {
    let batch = u32::try_from(MAX_SUBMIT_ITEMS).unwrap_or(u32::MAX);
    while !store.stage_pending(batch, now())?.is_empty() {}
    Ok(())
}

/// Removes events and idempotency records past retention.
fn prune(store: &mut Store, limits: &crate::config::Limits) -> Result<(), StoreError> {
    let since = now()
        .unix_millis()
        .saturating_sub(i64::try_from(limits.command_retention.as_millis()).unwrap_or(i64::MAX));
    store.prune(
        limits.event_retention.get(),
        UtcTimestamp::from_unix_millis(since),
    )?;
    Ok(())
}

/// Picks up to `free` jobs to run, other than `excluded`, moving queued
/// ones to resolving.
fn admit(store: &mut Store, free: usize, excluded: &[JobId]) -> Vec<Admission> {
    let Ok(ready) = store.admissible(free, excluded) else {
        return Vec::new();
    };
    ready
        .into_iter()
        .filter_map(|mut job| {
            if job.stage == JobStage::Queued {
                store
                    .advance_job(job.job, JobStage::Resolving, job.generation, None, now())
                    .ok()?;
                job.stage = JobStage::Resolving;
            }
            Some(job)
        })
        .collect()
}

/// Settles a job whose download task ended while it was still active: a
/// pause or cancel is completed, and an interrupted job that should run
/// again (resumed before its download stopped) is queued under a new
/// generation. A download that gave up for any other reason leaves the job
/// to restart recovery rather than retrying it at once. Returns whether the
/// job was left that way.
fn settle(store: &mut Store, job: JobId, interrupted: bool) -> Result<bool, StoreError> {
    let state = store.job_state(job)?;
    let (stage, generation) = (state.stage(), state.generation());
    if !stage.is_active() {
        return Ok(false);
    }
    // Publication is never interrupted; one left unfinished is reconciled
    // by restart recovery.
    if stage == JobStage::Publishing {
        return Ok(true);
    }
    let to = match state.desired() {
        DesiredState::Paused => JobStage::Paused,
        DesiredState::Cancelled => JobStage::Cancelled,
        DesiredState::Running if !interrupted => return Ok(true),
        DesiredState::Running => {
            if stage == JobStage::Transferring {
                store.advance_job(job, JobStage::Resolving, generation, None, now())?;
            }
            JobStage::Queued
        }
    };
    store.advance_job(job, to, generation, None, now())?;
    Ok(false)
}

type StoreWork = Box<dyn FnOnce(&mut Store) + Send>;

/// Work for the coordinator from its own tasks.
enum Internal {
    /// Store work from a download.
    Store(StoreWork),
    /// A download task ended, and whether it was interrupted.
    Finished(JobId, bool),
}

/// A download's way to the store: each call is queued to the coordinator
/// and runs between client requests.
#[derive(Clone)]
pub(crate) struct StoreClient(mpsc::Sender<Internal>);

impl StoreClient {
    /// Runs `work` on the store. `None` if the engine stopped first.
    pub(crate) async fn call<R: Send + 'static>(
        &self,
        work: impl FnOnce(&mut Store) -> R + Send + 'static,
    ) -> Option<R> {
        let (reply, result) = oneshot::channel();
        let work: StoreWork = Box::new(move |store| {
            let _ = reply.send(work(store));
        });
        self.0.send(Internal::Store(work)).await.ok()?;
        result.await.ok()
    }
}

#[cfg(test)]
impl StoreClient {
    /// A client served by a plain loop over `store`, to drive one download
    /// without an engine.
    pub(crate) fn serve(mut store: Store) -> Self {
        let (requests, mut inbox) = mpsc::channel(1);
        tokio::spawn(async move {
            while let Some(internal) = inbox.recv().await {
                if let Internal::Store(work) = internal {
                    work(&mut store);
                }
            }
        });
        Self(requests)
    }
}

/// Serializes requests against engine state. The only owner of the store.
struct Coordinator {
    identity: Identity,
    inbox: mpsc::Receiver<Request>,
    internal: mpsc::Receiver<Internal>,
    client: StoreClient,
    store: Option<Store>,
    supervisor: Supervisor,
    config: EngineConfig,
    transport: Option<Arc<dyn Transport>>,
    /// Running downloads and what stops each.
    running: HashMap<JobId, CancellationToken>,
    /// Active jobs whose download gave up, left for restart recovery
    /// instead of being admitted again by this run. At most one entry per
    /// job.
    held: HashSet<JobId>,
    /// Live durable events for subscribers.
    events: broadcast::Sender<DurableEvent>,
    /// Position of the last event broadcast.
    published: u64,
    pruned_at: Instant,
}

impl Coordinator {
    async fn run(mut self) {
        // Nothing runs yet, so files no record names can be told apart.
        self.clean_staging(true).await;
        self.changed().await;
        loop {
            tokio::select! {
                biased;
                () = self.supervisor.cancel.cancelled() => break,
                Some(internal) = self.internal.recv() => self.handle_internal(internal).await,
                request = self.inbox.recv() => match request {
                    Some(request) => self.handle(request).await,
                    None => break,
                },
            }
        }
    }

    // A dropped receiver means the caller gave up; there is nothing to do.
    async fn handle(&mut self, request: Request) {
        let instance = self.identity.instance;
        match request {
            Request::Hello(reply) => {
                let head = self.with_store(Store::event_head).await;
                let _ = reply.send(head.map_err(ProtocolError::from).map(|h| self.hello(h)));
            }
            Request::Execute(envelope, reply) => {
                let target = match envelope.command {
                    Command::Pause { job, .. } | Command::Cancel { job, .. } => Some(job),
                    Command::Submit { .. } | Command::Resume { .. } => None,
                };
                let (result, interrupt) = self
                    .with_store(move |store| {
                        let result = store.execute(&envelope, instance, now());
                        if let Ok(CommandResult::Submitted { .. }) = result {
                            // The batch is durable whatever happens here;
                            // links left unstaged are staged at the next start.
                            let _ = stage_all(store);
                        }
                        // Interrupt only a job that should now stop: a
                        // replayed pause from before a resume changes nothing.
                        let interrupt = target.filter(|&job| {
                            result.is_ok()
                                && store
                                    .job_state(job)
                                    .is_ok_and(|s| s.desired() != DesiredState::Running)
                        });
                        (result, interrupt)
                    })
                    .await;
                if let Some(job) = interrupt {
                    if let Some(token) = self.running.get(&job) {
                        token.cancel();
                    } else if self.held.remove(&job) {
                        // Nothing runs to stop, so the pause or cancel is
                        // settled here.
                        let settled = self.with_store(move |store| settle(store, job, true)).await;
                        if settled.unwrap_or(true) {
                            self.held.insert(job);
                        }
                    }
                }
                let _ = reply.send(result);
                self.changed().await;
            }
            Request::Query(query, reply) => {
                let result = self
                    .with_store(move |store| store.query(&query, instance))
                    .await;
                let _ = reply.send(result);
            }
            Request::Subscribe(request, reply) => {
                let subscription = self.subscribe(request).await;
                let _ = reply.send(subscription.map_err(ProtocolError::from));
            }
            #[cfg(test)]
            Request::Run(work, reply) => {
                self.with_store(work).await;
                let _ = reply.send(());
                self.changed().await;
            }
        }
    }

    async fn handle_internal(&mut self, internal: Internal) {
        match internal {
            Internal::Store(work) => self.with_store(work).await,
            Internal::Finished(job, interrupted) => {
                self.running.remove(&job);
                // A failure leaves the job for restart recovery.
                let settled = self
                    .with_store(move |store| settle(store, job, interrupted))
                    .await;
                // A job that cannot be settled is held too: retrying a
                // storage fault at once would only repeat it.
                if settled.unwrap_or(true) {
                    self.held.insert(job);
                }
                // The download may have replaced its partial, or the job
                // may have been queued to start from zero.
                self.clean_staging(false).await;
            }
        }
        self.changed().await;
    }

    /// Removes staging files no job generation can reopen from every root
    /// that opens, and with `unrecorded` the files left without a record
    /// (see [`cleanup`]). A root that does not open is skipped, and a pass
    /// that stops early is continued by the next one.
    async fn clean_staging(&mut self, unrecorded: bool) {
        let (roots, instance) = (self.config.roots.clone(), self.identity.instance);
        self.with_store(move |store| {
            for root in roots {
                if let Ok(root) = Root::open(&root.path, root.id, instance) {
                    let _ = cleanup::sweep(store, &root, unrecorded);
                }
            }
        })
        .await;
    }

    /// After a state change: start downloads for free slots, broadcast new
    /// events, and prune history when due.
    async fn changed(&mut self) {
        self.admit().await;
        self.publish_events().await;
        if self.pruned_at.elapsed() >= PRUNE_INTERVAL {
            self.pruned_at = Instant::now();
            let limits = self.config.limits.clone();
            let _ = self.with_store(move |store| prune(store, &limits)).await;
        }
    }

    async fn admit(&mut self) {
        let Some(transport) = self.transport.clone() else {
            return;
        };
        let Some(root) = self
            .config
            .download_root
            .and_then(|id| self.config.root(id))
            .cloned()
        else {
            return;
        };
        let free = self
            .config
            .limits
            .active_files
            .get()
            .saturating_sub(self.running.len());
        if free == 0 {
            return;
        }
        let excluded: Vec<JobId> = self.running.keys().chain(&self.held).copied().collect();
        let admitted = self
            .with_store(move |store| admit(store, free, &excluded))
            .await;
        for job in admitted {
            let id = job.job;
            let cancel = self.supervisor.cancel.child_token();
            self.running.insert(id, cancel.clone());
            let download = Download {
                store: self.client.clone(),
                transport: transport.clone(),
                root: root.clone(),
                instance: self.identity.instance,
                boot_epoch: self.identity.boot_epoch,
                limits: self.config.limits.clone(),
                timeouts: self.config.timeouts,
                cancel,
                guard: self.supervisor.guard(),
                job,
            };
            let finished = self.client.0.clone();
            self.supervisor.spawn(async move {
                let interrupted = download.run().await;
                let _ = finished.send(Internal::Finished(id, interrupted)).await;
            });
        }
    }

    /// Broadcasts committed events not yet sent, a ring's worth at a time.
    async fn publish_events(&mut self) {
        let page = self.config.limits.event_buffer.get();
        loop {
            let after = self.published;
            let Ok(events) = self
                .with_store(move |store| store.events_after(after, page))
                .await
            else {
                return;
            };
            if events.is_empty() {
                return;
            }
            for (position, kind) in events {
                self.published = position;
                // No receivers is not an error.
                let _ = self.events.send(DurableEvent {
                    cursor: EventCursor {
                        instance: self.identity.instance,
                        position,
                    },
                    kind,
                });
            }
        }
    }

    async fn subscribe(&mut self, request: Subscribe) -> Result<Subscription, StoreError> {
        let (pruned_through, head) = self.with_store(Store::retained_events).await?;
        let hello = self.hello(head);
        let retained = Retained {
            instance: self.identity.instance,
            pruned_through,
            head,
        };
        let after = match retained.plan(request) {
            SubscriptionStart::Resync(reason) => return Ok(Subscription::resync(hello, reason)),
            SubscriptionStart::ReplayAfter(after) => after,
        };
        let limit = self.config.limits.event_buffer.get();
        if head - after > limit as u64 {
            return Ok(Subscription::resync(hello, ResyncReason::Lagged));
        }
        let events = self
            .with_store(move |store| store.events_after(after, limit))
            .await?;
        let instance = self.identity.instance;
        let events = events
            .into_iter()
            .map(|(position, kind)| DurableEvent {
                cursor: EventCursor { instance, position },
                kind,
            })
            .collect();
        // Every committed event was broadcast before this request was
        // handled, so newer ones reach this receiver.
        Ok(Subscription::replay(
            hello,
            after,
            events,
            self.events.subscribe(),
        ))
    }

    fn hello(&self, head: u64) -> Hello {
        let instance = self.identity.instance;
        Hello {
            instance,
            boot_epoch: self.identity.boot_epoch,
            protocol: ProtocolVersion::CURRENT,
            head: EventCursor {
                instance,
                position: head,
            },
            limits: TransportLimits::DEFAULT,
        }
    }

    /// Runs blocking database work off the async workers. Requests are
    /// handled one at a time, so the store is never shared.
    ///
    /// The blocking work holds the state-directory lock until the database
    /// connection is back or dropped. If the engine is abandoned at a
    /// shutdown deadline and the runtime then shuts down, the coordinator
    /// is dropped but this thread may still be committing; another engine
    /// must not open the directory until it ends.
    async fn with_store<R: Send + 'static>(
        &mut self,
        work: impl FnOnce(&mut Store) -> R + Send + 'static,
    ) -> R {
        let store = self.store.take().expect("the coordinator owns the store");
        let lock = self.supervisor.lock.clone();
        let joined = tokio::task::spawn_blocking(move || {
            // Declared first so that, on success or unwind, the connection
            // is dropped before the lock.
            let lock = lock;
            let mut store = store;
            let out = work(&mut store);
            (store, out, lock)
        })
        .await;
        match joined {
            Ok((store, out, _lock)) => {
                self.store = Some(store);
                out
            }
            // A panic in storage code fails the engine task.
            Err(e) => match e.try_into_panic() {
                Ok(panic) => std::panic::resume_unwind(panic),
                // Only a runtime shutdown cancels blocking work.
                Err(_) => std::future::pending().await,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn start(dir: &std::path::Path) -> Result<EngineOwner, StartError> {
        EngineBuilder::new(EngineConfig::new(dir), Handle::current())
            .start()
            .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stuck_task_bounds_shutdown_and_keeps_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let owner = start(dir.path()).await.unwrap();
        let (release, released) = oneshot::channel::<()>();
        // A task that ignores cancellation until released.
        owner.spawn(async move {
            let _ = released.await;
        });
        let report = owner
            .shutdown(ShutdownRequest {
                timeout: Some(Duration::from_millis(50)),
            })
            .await;
        assert!(!report.clean);
        assert_eq!(report.unfinished_tasks, 1);
        assert!(!report.state_dir_released);
        // Old work may still write, so the directory stays owned.
        assert!(matches!(
            start(dir.path()).await,
            Err(StartError::StateDir(StateDirError::Locked(_)))
        ));
        release.send(()).unwrap();
        // Once the task ends the lock is released.
        let mut owner = None;
        for _ in 0..100 {
            if let Ok(o) = start(dir.path()).await {
                owner = Some(o);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            owner
                .expect("lock released")
                .shutdown(ShutdownRequest::default())
                .await
                .clean
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn panicked_task_makes_shutdown_unclean() {
        let dir = tempfile::tempdir().unwrap();
        let owner = start(dir.path()).await.unwrap();
        let (done, panicked) = oneshot::channel::<()>();
        owner.spawn(async move {
            let _done = done;
            panic!("worker failed");
        });
        // The sender drops while the task unwinds.
        let _ = panicked.await;
        let report = owner.shutdown(ShutdownRequest::default()).await;
        assert!(!report.clean, "{report:?}");
        assert_eq!(report.failed_tasks, 1);
        assert_eq!(report.unfinished_tasks, 0);
        // The panic released its hold on the directory.
        assert!(report.state_dir_released);
        let restarted = start(dir.path()).await.unwrap();
        assert!(restarted.shutdown(ShutdownRequest::default()).await.clean);
    }

    /// Sends store work straight to the coordinator.
    fn run_on_store(
        handle: &EngineHandle,
        work: impl FnOnce(&mut Store) + Send + 'static,
    ) -> oneshot::Receiver<()> {
        let (reply, done) = oneshot::channel();
        handle.send(Request::Run(Box::new(work), reply)).unwrap();
        done
    }

    #[test]
    fn blocking_store_work_keeps_the_lock_after_the_runtime_shuts_down() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_owned();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (entered, in_store) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let report = runtime.block_on(async {
            let owner = start(&path).await.unwrap();
            let _done = run_on_store(&owner.handle(), move |store| {
                entered.send(()).unwrap();
                released.recv().unwrap();
                // Still writing after its engine was abandoned.
                store.stage_pending(1, now()).unwrap();
            });
            in_store.recv().unwrap();
            owner
                .shutdown(ShutdownRequest {
                    timeout: Some(Duration::from_millis(50)),
                })
                .await
        });
        assert!(!report.clean);
        assert!(!report.state_dir_released);
        // Dropping the runtime drops the coordinator, not the blocked thread.
        runtime.shutdown_timeout(Duration::from_millis(100));
        assert!(matches!(
            StateDir::open(&path),
            Err(StateDirError::Locked(_))
        ));
        release.send(()).unwrap();
        let mut reopened = None;
        for _ in 0..200 {
            if let Ok(dir) = StateDir::open(&path) {
                reopened = Some(dir);
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(reopened.is_some(), "lock released once the work ended");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn storage_panic_fails_the_engine() {
        let dir = tempfile::tempdir().unwrap();
        let owner = start(dir.path()).await.unwrap();
        let handle = owner.handle();
        let done = run_on_store(&handle, |_| panic!("injected storage fault"));
        // The coordinator died without answering.
        assert!(done.await.is_err());
        let mut readiness = handle.readiness.clone();
        let failed = readiness.wait_for(|r| *r == Readiness::Failed);
        tokio::time::timeout(Duration::from_secs(5), failed)
            .await
            .expect("engine marked failed")
            .unwrap();
        assert!(!handle.wait_until_ready().await);
        assert_eq!(
            handle.hello().await.unwrap_err().code,
            ErrorCode::ShuttingDown
        );
        let report = owner.shutdown(ShutdownRequest::default()).await;
        assert!(!report.clean);
        assert_eq!(report.failed_tasks, 1);
        assert_eq!(report.unfinished_tasks, 0);
        assert!(report.state_dir_released);
        assert_eq!(handle.readiness(), Readiness::Failed);
    }

    #[tokio::test]
    async fn invalid_queue_capacity_fails_before_touching_the_state_dir() {
        let parent = tempfile::tempdir().unwrap();
        let dir = parent.path().join("state");
        let mut config = EngineConfig::new(&dir);
        config.request_queue = std::num::NonZeroUsize::MAX;
        let err = EngineBuilder::new(config, Handle::current())
            .start()
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                StartError::Config(ConfigError::OutOfRange {
                    setting: "request_queue",
                    ..
                })
            ),
            "{err:?}"
        );
        assert!(!dir.exists());
        let mut config = EngineConfig::new(&dir);
        config.request_queue =
            std::num::NonZeroUsize::new(EngineConfig::MAX_REQUEST_QUEUE).unwrap();
        let owner = EngineBuilder::new(config, Handle::current())
            .start()
            .await
            .unwrap();
        assert!(owner.shutdown(ShutdownRequest::default()).await.clean);
    }

    #[tokio::test]
    async fn full_queue_is_overloaded_not_buffered() {
        let (requests, _inbox) = mpsc::channel(1);
        let (_ready, readiness) = watch::channel(Readiness::Ready);
        let handle = EngineHandle {
            requests,
            readiness,
            instance: InstanceId::generate(),
        };
        let (first, _) = oneshot::channel();
        handle.send(Request::Hello(first)).unwrap();
        let (second, _) = oneshot::channel();
        let err = handle.send(Request::Hello(second)).unwrap_err();
        assert_eq!(err.code, ErrorCode::Overloaded);
        assert!(err.code.is_retryable());
    }
}
