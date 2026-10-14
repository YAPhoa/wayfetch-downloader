//! Engine startup, readiness, handles, and bounded shutdown (architecture
//! brief section 4.6, ADR-31 and ADR-34).
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
//! (moving active jobs to recovery), and stages submitted inputs before the
//! engine reports ready.
//!
//! Shutdown stops admitting requests, cancels engine tasks, and waits up to a
//! deadline. Every engine task holds a reference to the state-directory lock,
//! so a task still running after the deadline keeps the directory locked and
//! the report says so; another engine cannot take over while old work may
//! still write.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use wayfetch_domain::time::UtcTimestamp;
use wayfetch_protocol::command::MAX_SUBMIT_ITEMS;
use wayfetch_protocol::event::EventCursor;
use wayfetch_protocol::{
    CommandEnvelope, CommandResult, ErrorCode, Hello, InstanceId, Permission, ProtocolError,
    ProtocolVersion, Query, QueryResult, TransportLimits,
};

use crate::config::{ConfigError, EngineConfig};
use crate::state_dir::{Identity, StateDir, StateDirError};
use crate::store::{Store, StoreError, StoreInfo};

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
#[derive(Debug)]
pub struct EngineBuilder {
    config: EngineConfig,
    runtime: Handle,
}

impl EngineBuilder {
    /// An engine that will run on `runtime`, which the host owns.
    pub fn new(config: EngineConfig, runtime: Handle) -> Self {
        Self { config, runtime }
    }

    /// Validates the configuration, takes ownership of the state directory,
    /// and starts the engine's tasks. Fails if another engine owns the
    /// directory.
    pub async fn start(self) -> Result<EngineOwner, StartError> {
        self.config.validate()?;
        let path = self.config.state_dir.clone();
        let (state_dir, store) = self
            .runtime
            .spawn_blocking(move || -> Result<_, StartError> {
                let state_dir = StateDir::open(&path)?;
                let mut store = Store::open(state_dir.path())?;
                store.recover_after_restart(now())?;
                stage_all(&mut store)?;
                Ok((state_dir, store))
            })
            .await??;
        let identity = state_dir.identity();
        let store_info = store.info().clone();
        let state_dir = Arc::new(state_dir);
        let (readiness_tx, readiness_rx) = watch::channel(Readiness::Starting);
        let (requests, inbox) = mpsc::channel(self.config.request_queue.get());
        let owner = EngineOwner {
            handle: EngineHandle {
                requests,
                readiness: readiness_rx,
                instance: identity.instance,
            },
            identity,
            store_info,
            config: self.config,
            runtime: self.runtime,
            cancel: CancellationToken::new(),
            tasks: TaskTracker::new(),
            failed_tasks: Arc::default(),
            readiness: readiness_tx,
            state_dir: Some(state_dir),
        };
        let coordinator = Coordinator {
            identity,
            inbox,
            cancel: owner.cancel.clone(),
            store: Some(store),
        };
        owner.spawn(coordinator.run());
        owner.readiness.send_replace(Readiness::Ready);
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

/// The running engine. Held by the application host.
#[derive(Debug)]
pub struct EngineOwner {
    handle: EngineHandle,
    identity: Identity,
    store_info: StoreInfo,
    config: EngineConfig,
    runtime: Handle,
    cancel: CancellationToken,
    tasks: TaskTracker,
    failed_tasks: Arc<AtomicUsize>,
    readiness: watch::Sender<Readiness>,
    state_dir: Option<Arc<StateDir>>,
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

    /// Spawns an engine task. The task keeps the state directory locked
    /// until it ends, and must stop promptly once the engine's cancellation
    /// token fires. A panic is counted and makes shutdown unclean.
    pub(crate) fn spawn<F>(&self, task: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let guard = self.state_dir.clone();
        let work = self.runtime.spawn(async move {
            let _guard = guard;
            task.await;
        });
        let failed = Arc::clone(&self.failed_tasks);
        self.tasks.spawn_on(
            async move {
                if work.await.is_err() {
                    failed.fetch_add(1, Ordering::Relaxed);
                }
            },
            &self.runtime,
        );
    }

    /// Stops admitting requests, cancels engine tasks, and waits for them up
    /// to the deadline. Awaiting this is the only durability guarantee;
    /// dropping the owner cancels tasks without waiting.
    pub async fn shutdown(mut self, request: ShutdownRequest) -> ShutdownReport {
        let started = Instant::now();
        let timeout = request.timeout.unwrap_or(self.config.shutdown_timeout);
        self.readiness.send_replace(Readiness::ShuttingDown);
        self.cancel.cancel();
        self.tasks.close();
        let finished = tokio::time::timeout(timeout, self.tasks.wait())
            .await
            .is_ok();
        let state_dir_released = self
            .state_dir
            .take()
            .is_some_and(|dir| Arc::try_unwrap(dir).is_ok());
        if finished {
            self.readiness.send_replace(Readiness::Stopped);
        }
        let failed_tasks = self.failed_tasks.load(Ordering::Relaxed);
        ShutdownReport {
            clean: finished && failed_tasks == 0,
            unfinished_tasks: self.tasks.len(),
            failed_tasks,
            state_dir_released,
            elapsed: started.elapsed(),
        }
    }
}

impl Drop for EngineOwner {
    fn drop(&mut self) {
        // Best effort only: a destructor cannot await durability.
        self.readiness.send_if_modified(|r| {
            let changed = *r == Readiness::Ready || *r == Readiness::Starting;
            if changed {
                *r = Readiness::ShuttingDown;
            }
            changed
        });
        self.cancel.cancel();
    }
}

enum Request {
    Hello(oneshot::Sender<Result<Hello, ProtocolError>>),
    Execute(
        Box<CommandEnvelope>,
        oneshot::Sender<Result<CommandResult, ProtocolError>>,
    ),
    Query(Query, oneshot::Sender<Result<QueryResult, ProtocolError>>),
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
    /// committed; a retry with the same idempotency key replays the result.
    /// `granted` is what the host authenticated the caller for.
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

/// Serializes requests against engine state. The only owner of the store.
struct Coordinator {
    identity: Identity,
    inbox: mpsc::Receiver<Request>,
    cancel: CancellationToken,
    store: Option<Store>,
}

impl Coordinator {
    async fn run(mut self) {
        loop {
            tokio::select! {
                biased;
                () = self.cancel.cancelled() => break,
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
                let boot_epoch = self.identity.boot_epoch;
                let head = self.with_store(Store::event_head).await;
                let _ = reply.send(head.map_err(ProtocolError::from).map(|position| Hello {
                    instance,
                    boot_epoch,
                    protocol: ProtocolVersion::CURRENT,
                    head: EventCursor { instance, position },
                    limits: TransportLimits::DEFAULT,
                }));
            }
            Request::Execute(envelope, reply) => {
                let result = self
                    .with_store(move |store| {
                        let result = store.execute(&envelope, instance, now());
                        if let Ok(CommandResult::Submitted { .. }) = result {
                            // The batch is durable whatever happens here;
                            // links left unstaged are staged at the next start.
                            let _ = stage_all(store);
                        }
                        result
                    })
                    .await;
                let _ = reply.send(result);
            }
            Request::Query(query, reply) => {
                let result = self
                    .with_store(move |store| store.query(&query, instance))
                    .await;
                let _ = reply.send(result);
            }
        }
    }

    /// Runs blocking database work off the async workers. Requests are
    /// handled one at a time, so the store is never shared.
    async fn with_store<R: Send + 'static>(
        &mut self,
        work: impl FnOnce(&mut Store) -> R + Send + 'static,
    ) -> R {
        let mut store = self.store.take().expect("the coordinator owns the store");
        let joined = tokio::task::spawn_blocking(move || {
            let out = work(&mut store);
            (store, out)
        })
        .await;
        match joined {
            Ok((store, out)) => {
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
                StartError::Config(ConfigError::RequestQueueTooLarge(usize::MAX))
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
