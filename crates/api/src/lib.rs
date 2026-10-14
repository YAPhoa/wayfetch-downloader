//! The authenticated HTTP/SSE adapter for the Wayfetch Downloader engine
//! (architecture brief sections 4.3, 8.5 and 11.1, ADR-21 and ADR-28).
//!
//! It translates HTTP into the engine's transport-neutral commands,
//! queries, and subscriptions and nothing more: validation, idempotency,
//! revision checks, authorization of each operation, and event replay all
//! happen in the engine, exactly as for every other host. The adapter owns
//! what is specific to HTTP: bearer-token authentication, request and
//! connection bounds, server-sent events, and a graceful drain.
//!
//! Nothing a client does with a connection reaches job execution. A closed
//! connection or event stream ends only that connection; accepted jobs
//! keep running until a command says otherwise.

mod auth;
mod error;
mod routes;
mod server;

use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::middleware;
use axum::routing::{get, post};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use wayfetch_engine::EngineHandle;

pub use auth::{TokenFileError, Tokens};
pub use routes::cursor_id;

/// Bounds on what clients can make the server hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiLimits {
    /// Connections served at once, event streams included. Further clients
    /// wait in the kernel's backlog.
    pub max_connections: NonZeroUsize,
    /// Event streams open at once; more are refused with `overloaded`.
    /// Keep it below `max_connections`, so streams cannot take every
    /// connection from commands.
    pub max_event_streams: NonZeroUsize,
    /// Largest request body. Larger ones are refused with
    /// `limit_exceeded` without being read whole.
    pub max_body_bytes: usize,
    /// Time a client has to send a request's headers.
    pub header_read_timeout: Duration,
    /// Interval of keep-alive comments on idle event streams, which also
    /// notice clients that went away.
    pub keep_alive: Duration,
    /// How long shutdown waits for open connections to finish.
    pub drain_timeout: Duration,
}

impl ApiLimits {
    /// Defaults for a single-owner instance: initial targets, not
    /// measurements.
    pub const DEFAULT: Self = Self {
        max_connections: NonZeroUsize::new(64).unwrap(),
        max_event_streams: NonZeroUsize::new(16).unwrap(),
        // A thousand-URL submit of typical URLs; a client sends larger
        // batches in parts.
        max_body_bytes: 1024 * 1024,
        header_read_timeout: Duration::from_secs(10),
        keep_alive: Duration::from_secs(15),
        drain_timeout: Duration::from_secs(5),
    };
}

impl Default for ApiLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The HTTP API of one engine.
#[derive(Debug)]
pub struct Api {
    router: Router,
    limits: ApiLimits,
    shutdown: CancellationToken,
}

impl Api {
    /// The API for `engine`, admitting `tokens`.
    pub fn new(engine: EngineHandle, tokens: Tokens, limits: ApiLimits) -> Self {
        let shutdown = CancellationToken::new();
        let state = routes::AppState {
            engine,
            streams: Arc::new(Semaphore::new(limits.max_event_streams.get())),
            shutdown: shutdown.clone(),
            keep_alive: limits.keep_alive,
        };
        let router = Router::new()
            .route("/v1/capabilities", get(routes::capabilities))
            .route("/v1/inputs", post(routes::submit))
            .route("/v1/jobs", get(routes::list_jobs))
            .route("/v1/jobs/{id}", get(routes::inspect_job))
            .route("/v1/jobs/{id}/commands", post(routes::job_command))
            .route("/v1/events", get(routes::events))
            .route_layer(middleware::from_fn_with_state(
                Arc::new(tokens),
                auth::authenticate,
            ))
            .layer(DefaultBodyLimit::max(limits.max_body_bytes))
            .with_state(state);
        Self {
            router,
            limits,
            shutdown,
        }
    }

    /// Serves `listener` until `stop` completes, then ends event streams,
    /// stops accepting, and lets requests in progress finish for at most
    /// [`ApiLimits::drain_timeout`]. Returns whether every connection
    /// closed in time. Engine shutdown is the host's next step.
    pub async fn serve(self, listener: TcpListener, stop: impl Future<Output = ()>) -> bool {
        let shutdown = self.shutdown.clone();
        let (drained, ()) = tokio::join!(
            server::serve(listener, self.router, &self.limits, self.shutdown),
            async move {
                stop.await;
                shutdown.cancel();
            }
        );
        drained
    }
}
