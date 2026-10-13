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
//! Shutdown stops admitting requests, cancels engine tasks, and waits up to a
//! deadline. Every engine task holds a reference to the state-directory lock,
//! so a task still running after the deadline keeps the directory locked and
//! the report says so; another engine cannot take over while old work may
//! still write.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use wayfetch_protocol::event::EventCursor;
use wayfetch_protocol::{
    ErrorCode, Hello, InstanceId, ProtocolError, ProtocolVersion, TransportLimits,
};

use crate::config::EngineConfig;
use crate::state_dir::{Identity, StateDir, StateDirError};

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
    /// The state directory is locked, missing, or unreadable.
    #[error(transparent)]
    StateDir(#[from] StateDirError),
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

    /// Takes ownership of the state directory and starts the engine's
    /// tasks. Fails if another engine owns the directory.
    pub async fn start(self) -> Result<EngineOwner, StartError> {
        let path = self.config.state_dir.clone();
        let state_dir = self
            .runtime
            .spawn_blocking(move || StateDir::open(&path))
            .await??;
        let identity = state_dir.identity();
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
            config: self.config,
            runtime: self.runtime,
            cancel: CancellationToken::new(),
            tasks: TaskTracker::new(),
            readiness: readiness_tx,
            state_dir: Some(state_dir),
        };
        let coordinator = Coordinator {
            identity,
            inbox,
            cancel: owner.cancel.clone(),
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
    /// Every engine task finished within the deadline.
    pub clean: bool,
    /// Engine tasks still running at the deadline.
    pub unfinished_tasks: usize,
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
    config: EngineConfig,
    runtime: Handle,
    cancel: CancellationToken,
    tasks: TaskTracker,
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

    /// Spawns an engine task. The task keeps the state directory locked
    /// until it ends, and must stop promptly once the engine's cancellation
    /// token fires.
    pub(crate) fn spawn<F>(&self, task: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let guard = self.state_dir.clone();
        self.tasks.spawn_on(
            async move {
                let _guard = guard;
                task.await;
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
        let clean = tokio::time::timeout(timeout, self.tasks.wait())
            .await
            .is_ok();
        let state_dir_released = self
            .state_dir
            .take()
            .is_some_and(|dir| Arc::try_unwrap(dir).is_ok());
        if clean {
            self.readiness.send_replace(Readiness::Stopped);
        }
        ShutdownReport {
            clean,
            unfinished_tasks: self.tasks.len(),
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
    Hello(oneshot::Sender<Hello>),
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
        response.await.map_err(|_| shutting_down())
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

/// Serializes requests against engine state.
struct Coordinator {
    identity: Identity,
    inbox: mpsc::Receiver<Request>,
    cancel: CancellationToken,
}

impl Coordinator {
    async fn run(mut self) {
        loop {
            tokio::select! {
                biased;
                () = self.cancel.cancelled() => break,
                request = self.inbox.recv() => match request {
                    Some(request) => self.handle(request),
                    None => break,
                },
            }
        }
    }

    fn handle(&mut self, request: Request) {
        match request {
            Request::Hello(reply) => {
                // A dropped receiver means the caller gave up; nothing to do.
                let _ = reply.send(Hello {
                    instance: self.identity.instance,
                    boot_epoch: self.identity.boot_epoch,
                    protocol: ProtocolVersion::CURRENT,
                    // The durable outbox arrives with the store; until then
                    // the event sequence is empty.
                    head: EventCursor {
                        instance: self.identity.instance,
                        position: 0,
                    },
                    limits: TransportLimits::DEFAULT,
                });
            }
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
