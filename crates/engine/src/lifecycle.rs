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
//! then queued jobs while fewer downloads run than the effective most
//! downloads at once (the operator's policy applied to the user's saved
//! preference), reading only as many from the database as it has free
//! slots; each runs as a download task whose store calls come back to the
//! coordinator. Lowering that limit never stops a running download: new
//! ones wait until fewer run than the new limit.
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
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

use tokio::runtime::Handle;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use wayfetch_domain::failure::FailureCode;
use wayfetch_domain::id::{AccountId, JobId, RootId};
use wayfetch_domain::job::{DesiredState, JobStage};
use wayfetch_domain::name::DiagnosticText;
use wayfetch_domain::time::UtcTimestamp;
use wayfetch_protocol::account::{AccountSummary, Authorization, AuthorizationId};
use wayfetch_protocol::command::{IdempotencyKey, MAX_SUBMIT_ITEMS};
use wayfetch_protocol::event::{
    DurableEvent, EventCursor, ResyncReason, Retained, SubscriptionStart,
};
use wayfetch_protocol::query::{
    BoundedSetting, FailureSummary, FolderListing, JobRoute, RootSummary, SpeedLimit,
    SpeedLimitSetting,
};
use wayfetch_protocol::{
    Command, CommandEnvelope, CommandResult, ErrorCode, Hello, InstanceId, Permission,
    ProtocolError, ProtocolVersion, Query, QueryResult, Subscribe, TransportLimits,
};

mod accounts;
mod resolve;

pub(crate) use resolve::{Resolution, ResolveRequest, ResolvedLink};

use crate::accounts::provider::AccountProvider;
use crate::accounts::vault::Vault;
use crate::cleanup;
use crate::config::{ConfigError, EngineConfig, Preference};
use crate::destinations::{self, Claims, Destination, Destinations, Probes};
use crate::download::Download;
use crate::fs::Root;
use crate::state_dir::{Identity, StateDir, StateDirError};
use crate::store::{Admission, CommandContext, Store, StoreError, StoreInfo, Unusable};
use crate::subscription::Subscription;
use crate::transfer::throttle::Bandwidth;
use crate::transfer::transport::Transport;
use crate::{preferences, published};

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
    providers: Vec<Arc<dyn AccountProvider>>,
}

impl std::fmt::Debug for EngineBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineBuilder")
            .field("config", &self.config)
            .field("transport", &self.transport.is_some())
            .field("providers", &self.providers.len())
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
            providers: Vec::new(),
        }
    }

    /// Adds an account integration: accounts of its service can then be
    /// signed in. A later one for the same service replaces it.
    pub fn account_provider(mut self, provider: Arc<dyn AccountProvider>) -> Self {
        self.providers.retain(|p| p.service() != provider.service());
        self.providers.push(provider);
        self
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
        let config = self.config.clone();
        let (state_dir, store, head, destinations, saved, vault) = self
            .runtime
            .spawn_blocking(move || -> Result<_, StartError> {
                let state_dir = StateDir::open(&config.state_dir)?;
                let mut store = Store::open(state_dir.path())?;
                store.recover_after_restart(now())?;
                stage_all(&mut store)?;
                prune(&mut store, &config.limits)?;
                let head = store.event_head()?;
                let destinations = Destinations::load(&mut store, &config)?;
                let saved = store.settings()?;
                let vault = Vault::open(config.credential_key.as_deref(), store.key_check()?);
                Ok((state_dir, store, head, destinations, saved, vault))
            })
            .await??;
        let identity = state_dir.identity();
        let speed_limit = saved.download_speed_limit;
        let bandwidth = Arc::new(Bandwidth::new(
            preferences::resolve_speed_limit(&self.config.policy.download_speed_limit, speed_limit)
                .value,
        ));
        let credential_key = self.config.credential_key.clone();
        let store_info = store.info().clone();
        let (readiness_tx, readiness_rx) = watch::channel(Readiness::Starting);
        let (requests, inbox) = mpsc::channel(self.config.request_queue.get());
        // Each download waits for its one store call, so this never fills
        // beyond the number of downloads plus their completions.
        let most_downloads =
            usize::try_from(self.config.policy.max_active_downloads.max).unwrap_or(usize::MAX);
        let (internal_tx, internal) = mpsc::channel(most_downloads.saturating_mul(2));
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
        let (probes, sweeps) = (
            Probes::new(supervisor.guard()),
            Probes::new(supervisor.guard()),
        );
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
            destinations,
            max_active: saved.max_active_downloads,
            speed_limit,
            bandwidth,
            probes,
            file_jobs: Claims::default(),
            registering: 0,
            identifying: false,
            settings_waiting: Vec::new(),
            sweeps,
            events,
            published: head,
            pruned_at: Instant::now(),
            accounts: accounts::AccountService::new(self.providers, vault, credential_key),
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
    RegisterRoot(PathBuf, oneshot::Sender<Result<RootSummary, ProtocolError>>),
    BrowseFolders(
        Option<PathBuf>,
        oneshot::Sender<Result<FolderListing, ProtocolError>>,
    ),
    PublishedPath(JobId, oneshot::Sender<Result<PathBuf, ProtocolError>>),
    RefreshAccount(
        AccountId,
        oneshot::Sender<Result<AccountSummary, ProtocolError>>,
    ),
    CancelAuthorization(
        AuthorizationId,
        oneshot::Sender<Result<Authorization, ProtocolError>>,
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
        for required in envelope.command.required_permissions() {
            authorize(required, granted)?;
        }
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

    /// Permits the existing directory `path` as a destination on the
    /// user's explicit request, such as a folder chosen in a native picker
    /// or a server folder the owner chose in the browser, initializing it
    /// as a root unless it already is one. The root stays permitted across
    /// restarts; choosing it as the default is a separate settings change.
    /// The state directory, or a folder inside it, is refused. Idempotent.
    pub async fn register_root(
        &self,
        path: PathBuf,
        granted: &[Permission],
    ) -> Result<RootSummary, ProtocolError> {
        authorize(Permission::Control, granted)?;
        let (reply, response) = oneshot::channel();
        self.send(Request::RegisterRoot(path, reply))?;
        response.await.map_err(|_| shutting_down())?
    }

    /// The folders inside the folder at `path` on the engine's host (the
    /// top of the filesystem if `None`), for the owner to choose one to
    /// register ([`Self::register_root`]). Needs the same permission.
    pub async fn browse_folders(
        &self,
        path: Option<PathBuf>,
        granted: &[Permission],
    ) -> Result<FolderListing, ProtocolError> {
        authorize(Permission::Control, granted)?;
        let (reply, response) = oneshot::channel();
        self.send(Request::BrowseFolders(path, reply))?;
        response.await.map_err(|_| shutting_down())?
    }

    /// Where a listed, complete job's file is on this computer, after
    /// checking that its folder is mounted and the file is still the one
    /// Wayfetch published, for a host to open or show it on its user's
    /// request. Only for hosts that open files where the engine runs, such
    /// as the desktop app; never sent to a remote client. The file can
    /// still change after the check.
    pub async fn published_path(
        &self,
        job: JobId,
        granted: &[Permission],
    ) -> Result<PathBuf, ProtocolError> {
        authorize(Permission::Read, granted)?;
        let (reply, response) = oneshot::channel();
        self.send(Request::PublishedPath(job, reply))?;
        response.await.map_err(|_| shutting_down())?
    }

    /// Checks an account with its provider now, refreshing its credentials
    /// if they expired, and answers with what the provider said. Requests
    /// for an account already being checked share that check. Refused for
    /// a disabled account, and while credential storage is locked.
    pub async fn refresh_account(
        &self,
        account: AccountId,
        granted: &[Permission],
    ) -> Result<AccountSummary, ProtocolError> {
        authorize(Permission::Accounts, granted)?;
        let (reply, response) = oneshot::channel();
        self.send(Request::RefreshAccount(account, reply))?;
        response.await.map_err(|_| shutting_down())?
    }

    /// Cancels a sign-in still waiting for the person's approval, and
    /// answers with it; one that already ended is left as it is.
    pub async fn cancel_authorization(
        &self,
        id: AuthorizationId,
        granted: &[Permission],
    ) -> Result<Authorization, ProtocolError> {
        authorize(Permission::Accounts, granted)?;
        let (reply, response) = oneshot::channel();
        self.send(Request::CancelAuthorization(id, reply))?;
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

/// Jobs one admission pass may block before it yields to other work. The
/// pass then asks for another, so jobs behind any number of blocked ones
/// still start.
const ADMIT_BLOCK_BUDGET: usize = 64;

/// Picks up to `free` jobs to run, other than `excluded`, with where each
/// is saved, moving queued ones to resolving. A job submitted without a
/// destination belongs to the operator's download root, never to a
/// default chosen since; only an engine with no download root gives such
/// a job the default, since it was meant for none. A job whose root is no longer permitted, or that
/// has none while the download root cannot be identified, is blocked, and
/// the jobs behind it are looked at instead; without any download root,
/// or while configured roots are being looked for (`identifying`), such a
/// job waits. Also returns whether jobs are left to look at because
/// the pass blocked [`ADMIT_BLOCK_BUDGET`] of them.
/// Why a destination cannot be chosen: it is not permitted (`None`), or
/// not available now.
fn unusable_destination(problem: Option<String>) -> ProtocolError {
    match problem {
        None => ProtocolError::new(ErrorCode::NotFound, "destination not found"),
        Some(problem) => ProtocolError::new(
            ErrorCode::NotApplicable,
            &format!("the destination is unavailable: {problem}"),
        ),
    }
}

fn admit(
    store: &mut Store,
    free: usize,
    excluded: &[JobId],
    roots: &Destinations,
    identifying: bool,
) -> (Vec<(Admission, Destination)>, bool) {
    let fallback = if roots.has_download_root() {
        roots.download_root()
    } else {
        roots.default_root()
    };
    let unplaced = fallback.is_none() && roots.fallback_pending() && !identifying;
    let block = |store: &mut Store, job: &Admission, why: &str| {
        let failure = FailureSummary {
            code: FailureCode::DestinationUnavailable,
            retry_at: None,
            message: Some(DiagnosticText::new(why)),
        };
        let _ = store.record_stage(
            job.job,
            JobStage::Blocked,
            job.generation,
            Some(failure),
            now(),
        );
    };
    // Every job looked at is skipped by later reads in this pass, whether
    // it started, was blocked, or could not be changed.
    let mut skipped = excluded.to_vec();
    let mut admitted = Vec::new();
    let mut blocked = 0;
    while admitted.len() < free {
        let wanted = free - admitted.len();
        let Ok(ready) = store.admissible(wanted, &skipped, fallback.is_some() || unplaced) else {
            break;
        };
        let last = ready.len() < wanted;
        for mut job in ready {
            skipped.push(job.job);
            let Some(id) = job.root.or(fallback) else {
                block(
                    store,
                    &job,
                    "the download folder is not initialized, or its drive is not mounted",
                );
                blocked += 1;
                continue;
            };
            let Some(root) = roots.get(id) else {
                block(
                    store,
                    &job,
                    "the job's destination folder is no longer permitted",
                );
                blocked += 1;
                continue;
            };
            if job.root.is_none() {
                if store.assign_root(job.job, id, now()).is_err() {
                    continue;
                }
                job.root = Some(id);
            }
            if job.stage == JobStage::Queued {
                if store
                    .advance_job(job.job, JobStage::Resolving, job.generation, None, now())
                    .is_err()
                {
                    continue;
                }
                job.stage = JobStage::Resolving;
            }
            admitted.push((job, root));
        }
        if last {
            break;
        }
        if blocked >= ADMIT_BLOCK_BUDGET {
            return (admitted, true);
        }
    }
    (admitted, false)
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

/// Proof that the coordinator admitted a job while no earlier task of it
/// ran in this process. Only [`Coordinator::admit`] creates one, for a job
/// absent from `running`; a job leaves `running` only once its download
/// task has returned, and a download awaits every file write and sync it
/// starts and drops its files before it returns. So no writer of this
/// process still holds a staging file the job's earlier generations wrote,
/// which is what lets a retried attempt adopt one without the boot-epoch
/// rule (section 7.6, [`Store::adopt_retained`]).
#[derive(Debug, Clone)]
pub(crate) struct SoleAttempt {
    job: JobId,
}

impl SoleAttempt {
    /// The proof for `job`, or `None` while a task of it still runs.
    fn admitted(running: &HashMap<JobId, CancellationToken>, job: JobId) -> Option<Self> {
        (!running.contains_key(&job)).then_some(Self { job })
    }

    /// The job admitted.
    pub(crate) fn job(&self) -> JobId {
        self.job
    }

    /// A proof for a test that drives one download without a coordinator,
    /// and so runs no other task of the job.
    #[cfg(test)]
    pub(crate) fn assume(job: JobId) -> Self {
        Self { job }
    }
}

type StoreWork = Box<dyn FnOnce(&mut Store) + Send>;

/// Work for the coordinator from its own tasks.
enum Internal {
    /// Store work from a download.
    Store(StoreWork),
    /// A download task ended, and whether it was interrupted.
    Finished(JobId, bool),
    /// An admission pass stopped early; admit again.
    Admit,
    /// Configured roots found by their sentinels.
    Identified(Vec<(PathBuf, RootId)>),
    /// A folder's registration ended.
    Registered {
        path: PathBuf,
        result: Result<RootId, ProtocolError>,
        reply: oneshot::Sender<Result<RootSummary, ProtocolError>>,
    },
    /// A provider started an authorization, or could not.
    AccountBegun {
        key: IdempotencyKey,
        target: accounts::Target,
        result: Result<
            crate::accounts::provider::DeviceAuthorization,
            crate::accounts::provider::ProviderError,
        >,
    },
    /// The person approved an authorization.
    AccountAuthorized {
        flow: AuthorizationId,
        credential: crate::accounts::Secret,
        profile: crate::accounts::provider::Profile,
    },
    /// An authorization ended without approval.
    AccountFlowEnded {
        flow: AuthorizationId,
        state: wayfetch_protocol::account::AuthorizationState,
        problem: Option<String>,
    },
    /// A check of an account ended.
    AccountChecked {
        account: AccountId,
        generation: wayfetch_domain::version::Generation,
        result: crate::accounts::provider::Checked,
    },
    /// A download asks for its provider link.
    Resolve(ResolveRequest),
    /// A provider call for a download's link ended.
    Resolved(Box<resolve::Resolved>),
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

    /// Asks the coordinator for a routed job's download link. `None` if the
    /// engine stopped, or the work is stale.
    pub(crate) async fn resolve(
        &self,
        job: JobId,
        generation: wayfetch_domain::version::Generation,
        link: url::Url,
        refused: Option<i64>,
    ) -> Option<Resolution> {
        let (reply, answer) = oneshot::channel();
        let request = ResolveRequest {
            job,
            generation,
            link,
            refused,
            reply,
        };
        self.0.send(Internal::Resolve(request)).await.ok()?;
        answer.await.ok()
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
    /// Permitted roots and the default.
    destinations: Destinations,
    /// The user's saved most downloads at once, if any.
    max_active: Option<u64>,
    /// The user's saved total download speed limit, if any.
    speed_limit: Option<SpeedLimit>,
    /// The budget every download's body is written from, set to the
    /// effective speed limit.
    bandwidth: Arc<Bandwidth>,
    /// Filesystem checks of root folders.
    probes: Probes,
    /// Jobs whose published file a request is working on.
    file_jobs: Claims<JobId>,
    /// Folders being registered.
    registering: usize,
    /// Whether configured roots are being looked for.
    identifying: bool,
    /// Settings queries waiting for that look to end.
    settings_waiting: Vec<oneshot::Sender<Result<QueryResult, ProtocolError>>>,
    /// Roots being swept for abandoned staging files.
    sweeps: Probes,
    /// Live durable events for subscribers.
    events: broadcast::Sender<DurableEvent>,
    /// Position of the last event broadcast.
    published: u64,
    pruned_at: Instant,
    /// Accounts: the vault, sign-ins, and checks.
    accounts: accounts::AccountService,
}

impl Coordinator {
    async fn run(mut self) {
        self.refresh_destinations().await;
        // Cleanup passes and renames a crash interrupted, and looking for
        // configured roots, are finished before work starts and requests
        // are answered, unless a folder
        // does not answer in time; they then finish in the background, and
        // a job's rename is reconciled before its file is used.
        let mut pending = self.clean_staging();
        let files = self.files();
        let (reconciled, done) = oneshot::channel();
        self.supervisor.spawn(async move {
            files.reconcile().await;
            let _ = reconciled.send(());
        });
        pending.push(done);
        let finished = async {
            for pass in pending {
                let _ = pass.await;
            }
        };
        let deadline = tokio::time::sleep(destinations::PROBE_TIMEOUT);
        tokio::pin!(finished, deadline);
        // Configured roots are looked for too.
        let mut ended = false;
        while !(ended && !self.identifying) {
            tokio::select! {
                () = self.supervisor.cancel.cancelled() => return,
                () = &mut finished, if !ended => ended = true,
                () = &mut deadline => break,
                // That work goes through the store.
                Some(internal) = self.internal.recv() => self.handle_internal(internal).await,
            }
        }
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
            Request::Execute(envelope, reply)
                if matches!(envelope.command, Command::AuthorizeAccount { .. }) =>
            {
                self.authorize(envelope, reply).await;
            }
            Request::Execute(envelope, reply)
                if matches!(envelope.command, Command::Rename { .. }) =>
            {
                self.refresh_destinations().await;
                let files = self.files();
                self.answer(reply, async move { files.rename(*envelope).await });
            }
            Request::Execute(envelope, reply)
                if matches!(
                    envelope.command,
                    Command::Remove { .. } | Command::DeleteFiles { .. }
                ) =>
            {
                let id = match envelope.command {
                    Command::Remove { job, .. } | Command::DeleteFiles { job, .. } => job,
                    _ => unreachable!(),
                };
                if self.running.contains_key(&id) {
                    let checked = self
                        .with_store(move |store| store.check_remove_request(&envelope, instance))
                        .await;
                    let result = checked.and(Err(ProtocolError::new(
                        ErrorCode::Busy,
                        "the download is still stopping; try again shortly",
                    )));
                    let _ = reply.send(result);
                } else {
                    self.refresh_destinations().await;
                    let files = self.files();
                    self.answer(reply, async move { files.remove(*envelope).await });
                }
            }
            Request::Execute(envelope, reply) => {
                let target = match envelope.command {
                    Command::Pause { job, .. } | Command::Cancel { job, .. } => Some(job),
                    Command::Submit { .. }
                    | Command::Resume { .. }
                    | Command::Retry { .. }
                    | Command::UpdateSettings { .. }
                    | Command::Rename { .. }
                    | Command::DeleteFiles { .. }
                    | Command::Remove { .. }
                    | Command::RemoveRoot { .. }
                    | Command::CollectLinks { .. }
                    | Command::StartLinks { .. }
                    | Command::RemoveLinks { .. }
                    | Command::MoveLinks { .. }
                    | Command::AuthorizeAccount { .. }
                    | Command::SetAccountEnabled { .. }
                    | Command::RemoveAccount { .. } => None,
                };
                let account_command = matches!(
                    envelope.command,
                    Command::SetAccountEnabled { .. } | Command::RemoveAccount { .. }
                )
                .then(|| envelope.command.clone());
                let (unusable, route) = match self.prepare(&envelope).await {
                    Ok(prepared) => prepared,
                    Err(e) => {
                        let _ = reply.send(Err(e));
                        return;
                    }
                };
                let context = CommandContext {
                    destination: self.destinations.default_root(),
                    unusable,
                    route,
                };
                let settings = matches!(
                    envelope.command,
                    Command::UpdateSettings { .. } | Command::RemoveRoot { .. }
                );
                let removed = match envelope.command {
                    Command::RemoveRoot { root, .. } => Some(root),
                    _ => None,
                };
                let retried = match envelope.command {
                    Command::Retry { job, .. } => Some(job),
                    _ => None,
                };
                let (result, interrupt, current) = self
                    .with_store(move |store| {
                        let result = store.execute(&envelope, instance, &context, now());
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
                        let current = (settings && result.is_ok())
                            .then(|| store.settings().ok())
                            .flatten();
                        // A replayed removal may predate adding the folder
                        // again, so the store says whether it is gone.
                        let gone = removed.filter(|&root| {
                            result.is_ok()
                                && store.root_rows().is_ok_and(|rows| {
                                    !rows.iter().any(|r| r.id == root && r.registered)
                                })
                        });
                        (result, interrupt, (current, gone))
                    })
                    .await;
                let (current, gone) = current;
                if let Some(saved) = current {
                    self.destinations
                        .set_settings(saved.revision, saved.default_root);
                    // A lower limit takes effect as downloads end; a higher
                    // one admits at once, in `changed` below.
                    self.max_active = saved.max_active_downloads;
                    // Running transfers follow at their next grant, before
                    // the change is reported.
                    self.speed_limit = saved.download_speed_limit;
                    self.bandwidth.set(self.download_speed_limit().value);
                }
                if let Some(root) = gone {
                    self.destinations.remove(root);
                }
                // A retried job is queued, so one held for restart recovery
                // after a storage fault may run again.
                if let (Some(job), Ok(_)) = (retried, &result) {
                    self.held.remove(&job);
                }
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
                if let (Some(command), true) = (&account_command, result.is_ok()) {
                    self.account_changed(command).await;
                }
                let _ = reply.send(result);
                self.changed().await;
            }
            Request::Query(Query::Settings {}, reply) => self.settings(reply).await,
            Request::Query(Query::Accounts {}, reply) => self.list_accounts(reply).await,
            Request::Query(Query::Authorization { id }, reply) => self.authorization(id, reply),
            Request::RefreshAccount(account, reply) => self.refresh_account(account, reply).await,
            Request::CancelAuthorization(id, reply) => self.cancel_authorization(id, reply),
            Request::Query(query, reply) => {
                let result = self
                    .with_store(move |store| store.query(&query, instance))
                    .await;
                let _ = reply.send(result);
            }
            Request::RegisterRoot(path, reply) => self.register_root(path, reply),
            Request::BrowseFolders(path, reply) => {
                let probes = self.probes.clone();
                self.answer(
                    reply,
                    async move { destinations::browse(&probes, path).await },
                );
            }
            Request::PublishedPath(job, reply) => {
                self.refresh_destinations().await;
                let files = self.files();
                self.answer(reply, async move { files.locate(job).await });
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
                self.clean_staging();
            }
            // `changed` below admits.
            Internal::Admit => {}
            Internal::Identified(found) => {
                self.identifying = false;
                let waiting = std::mem::take(&mut self.settings_waiting);
                if !found.is_empty() {
                    let mut destinations = self.destinations.clone();
                    self.destinations = self
                        .with_store(move |store| {
                            for (path, id) in found {
                                // A sentinel naming a root recorded
                                // elsewhere stays unidentified; the
                                // operator decides.
                                let _ = destinations.adopt(store, path, id);
                            }
                            destinations
                        })
                        .await;
                }
                for reply in waiting {
                    self.answer_settings(reply);
                }
            }
            Internal::Registered {
                path,
                result,
                reply,
            } => {
                self.registering -= 1;
                let registered = result.map(|id| {
                    self.destinations.add(id, path.clone());
                    RootSummary {
                        id,
                        path: path.to_string_lossy().into_owned(),
                        available: true,
                        problem: None,
                        added: self.destinations.added(id),
                    }
                });
                let _ = reply.send(registered);
            }
            Internal::AccountBegun {
                key,
                target,
                result,
            } => self.begun(key, target, result),
            Internal::AccountAuthorized {
                flow,
                credential,
                profile,
            } => self.authorized(flow, credential, profile).await,
            Internal::AccountFlowEnded {
                flow,
                state,
                problem,
            } => self.flow_ended(flow, state, problem),
            Internal::AccountChecked {
                account,
                generation,
                result,
            } => self.checked(account, generation, result).await,
            Internal::Resolve(request) => self.resolve(request).await,
            Internal::Resolved(resolved) => self.resolved(resolved).await,
        }
        self.changed().await;
    }

    /// Checks a command before the store runs it. A submit or a collect
    /// without a chosen destination needs every configured root
    /// identified that can be, so the default is current. A settings
    /// change, and a destination chosen for links, must name a permitted
    /// root that is available now, and only a root the user added can be
    /// removed. A start of links learns which of their destinations cannot
    /// be used, which it returns, and the account it downloads through,
    /// which must be enabled, of a service that downloads, with credential
    /// storage unlocked. A retry of a command already recorded is only
    /// replayed.
    async fn prepare(
        &mut self,
        envelope: &CommandEnvelope,
    ) -> Result<(Unusable, Option<JobRoute>), ProtocolError> {
        let route = match &envelope.command {
            Command::StartLinks {
                account: Some(account),
                ..
            } if !self.replays(envelope).await? => Some(self.download_route(*account).await?),
            _ => None,
        };
        let unusable = self.prepare_destinations(envelope).await?;
        Ok((unusable, route))
    }

    /// The route for starting links through `account`, if it can download
    /// now; else why not, so the links stay collected. The store checks
    /// again when it starts them.
    async fn download_route(&mut self, account: AccountId) -> Result<JobRoute, ProtocolError> {
        let row = self.with_store(move |store| store.account(account)).await?;
        let supported = self
            .accounts
            .provider(&row.service)
            .is_some_and(|p| p.support().downloads);
        if !supported {
            return Err(ProtocolError::new(
                ErrorCode::NotApplicable,
                "Wayfetch cannot download through accounts of this service yet",
            ));
        }
        if !row.enabled {
            return Err(ProtocolError::new(
                ErrorCode::NotApplicable,
                "the chosen account is disabled; enable it or choose another way to download",
            ));
        }
        self.accounts.vault.keys()?;
        Ok(JobRoute {
            service: row.service,
            account,
        })
    }

    async fn prepare_destinations(
        &mut self,
        envelope: &CommandEnvelope,
    ) -> Result<Unusable, ProtocolError> {
        match &envelope.command {
            Command::Submit { .. }
            | Command::CollectLinks {
                destination: None, ..
            } => self.refresh_destinations().await,
            Command::UpdateSettings {
                default_root,
                max_active_downloads,
                download_speed_limit,
                ..
            } => {
                if self.replays(envelope).await? {
                    return Ok(Unusable::new());
                }
                if default_root.is_none()
                    && max_active_downloads.is_none()
                    && download_speed_limit.is_none()
                {
                    return Err(ProtocolError::new(
                        ErrorCode::InvalidRequest,
                        "the settings change changes nothing",
                    ));
                }
                if let Some(change) = *max_active_downloads {
                    preferences::check(
                        Preference::MAX_ACTIVE_DOWNLOADS,
                        &self.config.policy.max_active_downloads,
                        change,
                    )?;
                }
                if let Some(change) = *download_speed_limit {
                    preferences::check_speed_limit(
                        &self.config.policy.download_speed_limit,
                        change,
                    )?;
                }
                if let Some(root) = *default_root {
                    self.refresh_destinations().await;
                    self.usable(root).await.map_err(unusable_destination)?;
                }
            }
            Command::CollectLinks {
                destination: Some(root),
                ..
            }
            | Command::MoveLinks {
                destination: root, ..
            } => {
                if self.replays(envelope).await? {
                    return Ok(Unusable::new());
                }
                self.refresh_destinations().await;
                self.usable(*root).await.map_err(unusable_destination)?;
            }
            Command::StartLinks { links, .. } => {
                if self.replays(envelope).await? {
                    return Ok(Unusable::new());
                }
                self.refresh_destinations().await;
                let ids: Vec<_> = links.as_slice().iter().map(|l| l.link).collect();
                let roots = self.with_store(move |store| store.link_roots(&ids)).await?;
                let mut unusable = Unusable::new();
                for root in roots {
                    if let Err(problem) = self.usable(root).await {
                        let why = problem.unwrap_or_else(|| {
                            "it is no longer a permitted destination".to_owned()
                        });
                        unusable.insert(root, why);
                    }
                }
                return Ok(unusable);
            }
            // Only a folder the user added can be removed; the store checks
            // the rest.
            Command::RemoveRoot { root, .. } => {
                let root = *root;
                if self.replays(envelope).await? {
                    return Ok(Unusable::new());
                }
                self.refresh_destinations().await;
                if self.destinations.get(root).is_none() {
                    return Err(ProtocolError::new(
                        ErrorCode::NotFound,
                        "destination not found",
                    ));
                }
                if !self.destinations.added(root) {
                    return Err(ProtocolError::new(
                        ErrorCode::NotApplicable,
                        "the server's configuration names this folder, so it cannot be removed here",
                    ));
                }
            }
            // A job resumed or retried may be waiting for its root to be
            // found.
            Command::Resume { .. } | Command::Retry { .. } => self.identify_destinations(),
            Command::Pause { .. }
            | Command::Cancel { .. }
            | Command::Rename { .. }
            | Command::DeleteFiles { .. }
            | Command::Remove { .. }
            | Command::RemoveLinks { .. }
            | Command::AuthorizeAccount { .. }
            | Command::SetAccountEnabled { .. }
            | Command::RemoveAccount { .. } => {}
        }
        Ok(Unusable::new())
    }

    /// What applies for the most downloads at once.
    fn max_active_downloads(&self) -> BoundedSetting {
        preferences::resolve(
            Preference::MAX_ACTIVE_DOWNLOADS,
            &self.config.policy.max_active_downloads,
            self.max_active,
        )
    }

    /// What applies for the total download speed limit.
    fn download_speed_limit(&self) -> SpeedLimitSetting {
        preferences::resolve_speed_limit(&self.config.policy.download_speed_limit, self.speed_limit)
    }

    /// Whether root `id` is permitted (`Err(None)` if not) and available
    /// now (`Err(Some(why))` if not).
    fn usable(&self, id: RootId) -> impl Future<Output = Result<(), Option<String>>> + use<> {
        let (root, probes) = (self.destinations.get(id), self.probes.clone());
        let instance = self.identity.instance;
        async move {
            probes
                .check(root.ok_or(None)?, instance)
                .await
                .map_err(Some)
        }
    }

    /// Whether the store holds a result for this command, which it replays.
    async fn replays(&mut self, envelope: &CommandEnvelope) -> Result<bool, ProtocolError> {
        let key = envelope.idempotency_key.as_uuid().to_string();
        Ok(self
            .with_store(move |store| store.has_command(&key))
            .await?)
    }

    /// Answers the settings query once configured roots being looked for
    /// are found or given up on, within the probes' deadline.
    async fn settings(&mut self, reply: oneshot::Sender<Result<QueryResult, ProtocolError>>) {
        self.refresh_destinations().await;
        if !self.identifying {
            self.answer_settings(reply);
        } else if self.settings_waiting.len() >= self.config.request_queue.get() {
            let _ = reply.send(Err(ProtocolError::new(
                ErrorCode::Busy,
                "too many settings requests are waiting",
            )));
        } else {
            self.settings_waiting.push(reply);
        }
    }

    /// Answers a settings query. Roots are checked off the coordinator, so
    /// a hung mount delays only this answer.
    fn answer_settings(&self, reply: oneshot::Sender<Result<QueryResult, ProtocolError>>) {
        let (destinations, probes) = (self.destinations.clone(), self.probes.clone());
        let instance = self.identity.instance;
        let max_active_downloads = self.max_active_downloads();
        let download_speed_limit = self.download_speed_limit();
        self.supervisor.spawn(async move {
            let mut checks = tokio::task::JoinSet::new();
            for (i, root) in destinations.all().into_iter().enumerate() {
                let probes = probes.clone();
                checks.spawn(async move { (i, probes.check(root, instance).await) });
            }
            let mut available = checks.join_all().await;
            available.sort_by_key(|(i, _)| *i);
            let available = available.into_iter().map(|(_, a)| a).collect();
            let settings =
                destinations.settings(available, max_active_downloads, download_speed_limit);
            let _ = reply.send(Ok(QueryResult::Settings(settings)));
        });
    }

    /// What a request about published files works with.
    fn files(&self) -> published::Files {
        published::Files {
            store: self.client.clone(),
            probes: self.probes.clone(),
            jobs: self.file_jobs.clone(),
            roots: self.destinations.clone(),
            instance: self.identity.instance,
            cancel: self.supervisor.cancel.clone(),
        }
    }

    /// Answers a request with `work`, run as an engine task of its own.
    /// The work stops between its steps at shutdown, so it is waited for
    /// rather than abandoned mid-step.
    fn answer<T: Send + 'static>(
        &self,
        reply: oneshot::Sender<Result<T, ProtocolError>>,
        work: impl Future<Output = Result<T, ProtocolError>> + Send + 'static,
    ) {
        self.supervisor.spawn(async move {
            let _ = reply.send(work.await);
        });
    }

    /// Registers the folder at `path` as a permitted root in a task of its
    /// own, so a folder that hangs delays only this request.
    fn register_root(
        &mut self,
        path: PathBuf,
        reply: oneshot::Sender<Result<RootSummary, ProtocolError>>,
    ) {
        if !self.destinations.has_room_for(&path, self.registering) {
            let _ = reply.send(Err(destinations::RootError::TooMany.into()));
            return;
        }
        self.registering += 1;
        let (store, probes) = (self.client.clone(), self.probes.clone());
        let (instance, cancel) = (self.identity.instance, self.supervisor.cancel.clone());
        // Not abandoned on cancellation: it stops between steps instead,
        // and a hung step keeps the engine from shutting down cleanly.
        let state_dir = self.config.state_dir.clone();
        self.supervisor.spawn(async move {
            let result =
                destinations::register(&store, &probes, &cancel, path.clone(), instance, state_dir)
                    .await;
            if cancel.is_cancelled() {
                let _ = reply.send(Err(shutting_down()));
                return;
            }
            let _ = store
                .0
                .send(Internal::Registered {
                    path,
                    result,
                    reply,
                })
                .await;
        });
    }

    /// Identifies configured roots the store records, and starts looking
    /// for the sentinels of the rest ([`Self::identify_destinations`]).
    async fn refresh_destinations(&mut self) {
        self.recall_destinations().await;
        self.identify_destinations();
    }

    /// Identifies configured roots the store records. No filesystem access.
    async fn recall_destinations(&mut self) {
        if self.destinations.complete() {
            return;
        }
        let (mut destinations, probes) = (self.destinations.clone(), self.probes.clone());
        self.destinations = self
            .with_store(move |store| {
                destinations.recall(store, &probes);
                destinations
            })
            .await;
    }

    /// Starts reading the sentinels of configured roots not identified
    /// yet, in a task of its own, within the probes' deadline; one look at
    /// a time. What it finds arrives as [`Internal::Identified`].
    fn identify_destinations(&mut self) {
        if self.identifying || self.destinations.complete() {
            return;
        }
        self.identifying = true;
        let (paths, probes) = (self.destinations.unidentified(), self.probes.clone());
        let (store, instance) = (self.client.clone(), self.identity.instance);
        self.supervisor.spawn(async move {
            let mut checks = tokio::task::JoinSet::new();
            for path in paths {
                let probes = probes.clone();
                checks.spawn(async move {
                    let id = probes.identify(&path, instance).await;
                    id.map(|id| (path, id))
                });
            }
            let found = checks.join_all().await.into_iter().flatten().collect();
            let _ = store.0.send(Internal::Identified(found)).await;
        });
    }

    /// Starts removing staging files no job generation can reopen from
    /// every root that opens (see [`cleanup`]), a pass per root in a task
    /// of its own. A root still being swept is skipped, and a pass that
    /// stops early is continued by the next one. Returns when each pass
    /// ends.
    fn clean_staging(&mut self) -> Vec<oneshot::Receiver<()>> {
        let instance = self.identity.instance;
        let mut passes = Vec::new();
        for root in self.destinations.all() {
            let Some(slot) = self.sweeps.claim(&root.path) else {
                continue;
            };
            let (store, cancel) = (self.client.clone(), self.supervisor.cancel.clone());
            let (ended, pass) = oneshot::channel();
            passes.push(pass);
            let sweeps = self.sweeps.clone();
            // Stops between its steps at shutdown, never during one.
            self.supervisor.spawn(async move {
                let _slot = slot;
                let opened = sweeps
                    .blocking(move || Root::open(&root.path, root.id, instance))
                    .await;
                if let (Ok(Ok(root)), false) = (opened, cancel.is_cancelled()) {
                    let _ = cleanup::sweep(&store, &sweeps, &cancel, Arc::new(root)).await;
                }
                let _ = ended.send(());
            });
        }
        passes
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
        let limit = usize::try_from(self.max_active_downloads().value).unwrap_or(usize::MAX);
        let free = limit.saturating_sub(self.running.len());
        if free == 0 {
            return;
        }
        self.recall_destinations().await;
        let excluded: Vec<JobId> = self.running.keys().chain(&self.held).copied().collect();
        let (roots, identifying) = (self.destinations.clone(), self.identifying);
        let (admitted, more) = self
            .with_store(move |store| admit(store, free, &excluded, &roots, identifying))
            .await;
        if more {
            // After the requests already waiting.
            let _ = self.client.0.try_send(Internal::Admit);
        }
        for (job, root) in admitted {
            let id = job.job;
            // Admission excluded running jobs, so this always holds.
            let Some(attempt) = SoleAttempt::admitted(&self.running, id) else {
                continue;
            };
            let cancel = self.supervisor.cancel.child_token();
            self.running.insert(id, cancel.clone());
            let download = Download {
                store: self.client.clone(),
                transport: transport.clone(),
                root,
                instance: self.identity.instance,
                boot_epoch: self.identity.boot_epoch,
                limits: self.config.limits.clone(),
                timeouts: self.config.timeouts,
                bandwidth: self.bandwidth.clone(),
                cancel,
                guard: self.supervisor.guard(),
                job,
                attempt,
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
