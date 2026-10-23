//! The authenticated HTTP/SSE adapter for the engine (ADR-21, ADR-28).
//!
//! It translates HTTP into engine commands, queries, and subscriptions;
//! validation, idempotency, and authorization stay in the engine. It owns
//! only what is HTTP-specific: tokens, browser sessions, request bounds,
//! server-sent events, and a graceful drain. A closed connection never
//! affects jobs.

mod auth;
mod browser;
mod error;
mod routes;
mod secret;
mod server;
mod ui;

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
pub use browser::{
    BrowserAuth, BrowserAuthError, BrowserConfig, CSRF_HEADER, SetupIssued, reset_owner,
};
pub use routes::cursor_id;
pub use ui::{UiAssets, UiAssetsError};

/// A host's own readiness condition, beyond the engine's: for example,
/// whether the download root is usable. It runs on a blocking thread for
/// each readiness request and returns a short, fixed reason when not
/// ready. The reason is served without authentication, so it must not
/// carry paths, tokens, or other details.
pub type ReadinessCheck = Arc<dyn Fn() -> Result<(), &'static str> + Send + Sync>;

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
    /// Time a client has to send a request's body after its headers.
    /// Together with `header_read_timeout`, this bounds how long a client
    /// that stops sending can hold a connection.
    pub body_read_timeout: Duration,
    /// Interval of keep-alive comments on idle event streams, which also
    /// notice clients that went away.
    pub keep_alive: Duration,
    /// How long shutdown waits for open connections to finish.
    pub drain_timeout: Duration,
    /// How long a readiness request waits for its checks before it answers
    /// `check_timed_out`. Readiness checks run one at a time, so a check
    /// stuck in the filesystem never occupies more than one thread.
    pub readiness_timeout: Duration,
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
        body_read_timeout: Duration::from_secs(10),
        keep_alive: Duration::from_secs(15),
        drain_timeout: Duration::from_secs(5),
        // Below the 3 s `wayfetchd ready` allows, so it gets an answer.
        readiness_timeout: Duration::from_secs(2),
    };
}

impl Default for ApiLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The HTTP API of one engine.
pub struct Api {
    engine: EngineHandle,
    tokens: Tokens,
    browser: Option<Arc<BrowserAuth>>,
    ui: Option<UiAssets>,
    limits: ApiLimits,
    readiness_check: Option<ReadinessCheck>,
    shutdown: CancellationToken,
}

impl std::fmt::Debug for Api {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Api")
            .field("limits", &self.limits)
            .field("browser", &self.browser.is_some())
            .field("ui", &self.ui.as_ref().map(UiAssets::version))
            .field("readiness_check", &self.readiness_check.is_some())
            .finish_non_exhaustive()
    }
}

impl Api {
    /// The API for `engine`, admitting `tokens`.
    pub fn new(engine: EngineHandle, tokens: Tokens, limits: ApiLimits) -> Self {
        Self {
            engine,
            tokens,
            browser: None,
            ui: None,
            limits,
            readiness_check: None,
            shutdown: CancellationToken::new(),
        }
    }

    /// Adds `check` to what `GET /v1/ready` requires.
    #[must_use]
    pub fn readiness_check(mut self, check: ReadinessCheck) -> Self {
        self.readiness_check = Some(check);
        self
    }

    /// Also admits browsers signed in as the owner through `browser`.
    #[must_use]
    pub fn browser_access(mut self, browser: BrowserAuth) -> Self {
        self.browser = Some(Arc::new(browser));
        self
    }

    /// Also serves `ui` as the web UI at `/`.
    #[must_use]
    pub fn ui(mut self, ui: UiAssets) -> Self {
        self.ui = Some(ui);
        self
    }

    /// The routes. Every route but `/v1/ready`, `/v1/auth/*`, and the web
    /// UI's static files requires a bearer token or an owner session.
    /// Readiness answers anyone, so a container health check needs no
    /// credential, and it reveals only whether the instance is ready. The
    /// auth routes are how a browser gets a session, and the UI holds no
    /// data of its own.
    fn router(&self) -> Router {
        let state = routes::AppState {
            engine: self.engine.clone(),
            tokens: Arc::new(self.tokens.clone()),
            browser: self.browser.clone(),
            streams: Arc::new(Semaphore::new(self.limits.max_event_streams.get())),
            shutdown: self.shutdown.clone(),
            keep_alive: self.limits.keep_alive,
            body_timeout: self.limits.body_read_timeout,
            readiness_check: self.readiness_check.clone(),
            readiness_running: Arc::new(Semaphore::new(1)),
            readiness_timeout: self.limits.readiness_timeout,
        };
        let router = Router::new()
            .route("/v1/capabilities", get(routes::capabilities))
            .route("/v1/inputs", post(routes::submit))
            .route("/v1/jobs", get(routes::list_jobs))
            .route("/v1/jobs/{id}", get(routes::inspect_job))
            .route("/v1/job-counts", get(routes::count_jobs))
            .route("/v1/jobs/{id}/commands", post(routes::job_command))
            .route("/v1/jobs/{id}/extract", post(routes::extract))
            .route("/v1/jobs/{id}/source", get(routes::job_source))
            .route(
                "/v1/staging/links",
                get(routes::list_links).post(routes::collect_links),
            )
            .route("/v1/staging/links/{id}/source", get(routes::link_source))
            .route("/v1/staging/commands", post(routes::links_command))
            .route(
                "/v1/settings",
                get(routes::settings).put(routes::update_settings),
            )
            .route("/v1/link-filters/try", post(routes::try_link_filter))
            .route("/v1/folders", get(routes::folders))
            .route("/v1/roots", post(routes::add_root))
            .route("/v1/roots/remove", post(routes::remove_root))
            .route("/v1/plugins/change", post(routes::change_plugin))
            .route("/v1/updates", get(routes::updates))
            .route("/v1/updates/check", post(routes::check_for_updates))
            .merge(account_routes())
            .route("/v1/events", get(routes::events))
            .route_layer(middleware::from_fn_with_state(
                state.clone(),
                auth::authenticate,
            ))
            .route("/v1/ready", get(routes::ready))
            .merge(
                Router::new()
                    .route("/v1/auth/session", get(browser::routes::session))
                    .route("/v1/auth/setup", post(browser::routes::setup))
                    .route("/v1/auth/login", post(browser::routes::login))
                    .route("/v1/auth/logout", post(browser::routes::logout))
                    .layer(middleware::map_response(browser::routes::no_store)),
            )
            .layer(DefaultBodyLimit::max(self.limits.max_body_bytes))
            .with_state(state);
        match &self.ui {
            Some(ui) => router.merge(ui.router()),
            None => router,
        }
    }

    /// Serves `listener` until `stop` completes, then ends event streams,
    /// stops accepting, and lets requests in progress finish for at most
    /// [`ApiLimits::drain_timeout`]. Returns whether every connection
    /// closed in time. Engine shutdown is the host's next step.
    pub async fn serve(self, listener: TcpListener, stop: impl Future<Output = ()>) -> bool {
        let shutdown = self.shutdown.clone();
        let router = self.router();
        let (drained, ()) = tokio::join!(
            server::serve(listener, router, &self.limits, self.shutdown),
            async move {
                stop.await;
                shutdown.cancel();
            }
        );
        drained
    }
}

/// The account routes. Their answers are never cached: a sign-in carries
/// the code the person enters at the provider.
fn account_routes() -> Router<routes::AppState> {
    Router::new()
        .route("/v1/accounts", get(routes::accounts))
        .route(
            "/v1/accounts/authorizations",
            post(routes::authorize_account),
        )
        .route(
            "/v1/accounts/authorizations/{id}",
            get(routes::authorization),
        )
        .route(
            "/v1/accounts/authorizations/{id}/cancel",
            post(routes::cancel_authorization),
        )
        .route("/v1/accounts/{id}/commands", post(routes::account_command))
        .route("/v1/accounts/{id}/refresh", post(routes::refresh_account))
        .layer(middleware::map_response(browser::routes::no_store))
}
