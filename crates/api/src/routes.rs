//! The `/v1` routes: thin translations between HTTP and the engine's
//! transport-neutral commands, queries, and subscriptions.
//!
//! | Route | Engine operation |
//! |---|---|
//! | `GET /v1/capabilities` | handshake: instance, boot epoch, protocol, head, limits |
//! | `POST /v1/inputs` | [`Command::Submit`] |
//! | `GET /v1/jobs` | [`Query::ListJobs`] |
//! | `GET /v1/jobs/{id}` | [`Query::InspectJob`] |
//! | `GET /v1/job-counts` | [`Query::CountJobs`] |
//! | `POST /v1/jobs/{id}/commands` | [`Command::Pause`], [`Command::Resume`], [`Command::Cancel`], [`Command::Rename`], [`Command::Remove`] |
//! | `GET /v1/jobs/{id}/source` | [`Query::JobSource`] |
//! | `GET /v1/staging/links` | [`Query::ListLinks`]: links collected for review |
//! | `POST /v1/staging/links` | [`Command::CollectLinks`] |
//! | `POST /v1/staging/commands` | [`Command::StartLinks`], [`Command::RemoveLinks`], [`Command::MoveLinks`] |
//! | `GET /v1/staging/links/{id}/source` | [`Query::LinkSource`] |
//! | `GET /v1/settings` | [`Query::Settings`]: the default destination and the roots to choose from |
//! | `PUT /v1/settings` | [`Command::UpdateSettings`] |
//! | `GET /v1/folders` | [`EngineHandle::browse_folders`]: folders on the server, to choose one to add |
//! | `POST /v1/roots` | [`EngineHandle::register_root`]: permits a server folder as a destination |
//! | `POST /v1/roots/remove` | [`EngineHandle::execute`]: stops offering a folder the user added |
//! | `GET /v1/events` | subscription, as server-sent events |
//! | `GET /v1/ready` | readiness: lifecycle phase, a handshake, the host's check |
//!
//! Responses carry the protocol's own result and error types, so the JSON
//! is the same that desktop IPC carries. Commands name an idempotency key
//! and the target instance in their body, as in a
//! [`wayfetch_engine::protocol::CommandEnvelope`].

use std::convert::Infallible;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Extension, FromRequest, Path, Query as UrlQuery, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::{Stream, StreamExt};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use wayfetch_engine::domain::id::{JobId, PackageId, SourceLinkId};
use wayfetch_engine::domain::job::JobStage;
use wayfetch_engine::protocol::command::CommandResult;
use wayfetch_engine::protocol::query::{FolderListing, JobFilter, PageSize, RootSummary};
use wayfetch_engine::protocol::request::{
    AddRootRequest, CollectRequest, JobCommandRequest, LinksCommandRequest, RemoveRootRequest,
    SubmitRequest, UpdateSettingsRequest,
};
use wayfetch_engine::protocol::{
    CommandEnvelope, ErrorCode, EventCursor, Hello, InstanceId, Query, QueryResult, StreamItem,
    Subscribe,
};
use wayfetch_engine::{EngineHandle, Readiness};

use crate::ReadinessCheck;
use crate::auth::{Grant, Tokens};
use crate::browser::{BrowserAuth, SessionEnd};
use crate::error::ApiError;

/// What every handler shares.
#[derive(Clone)]
pub struct AppState {
    pub engine: EngineHandle,
    /// The bearer tokens admitted.
    pub tokens: Arc<Tokens>,
    /// Browser sign-in and sessions, if configured.
    pub browser: Option<Arc<BrowserAuth>>,
    /// One permit per open event stream.
    pub streams: Arc<Semaphore>,
    /// Ends every event stream when the server shuts down.
    pub shutdown: CancellationToken,
    pub keep_alive: Duration,
    /// Time a client has to send a request body once its headers arrived.
    pub body_timeout: Duration,
    /// The host's own readiness condition.
    pub readiness_check: Option<ReadinessCheck>,
    /// One permit, held by the readiness check running, if any.
    pub readiness_running: Arc<Semaphore>,
    /// How long a readiness request waits for its checks.
    pub readiness_timeout: Duration,
}

type ApiResult<T> = Result<Json<T>, ApiError>;

/// The handshake.
pub async fn capabilities(State(app): State<AppState>) -> ApiResult<Hello> {
    Ok(Json(app.engine.hello().await?))
}

/// The body of `GET /v1/ready`.
#[derive(Debug, Serialize)]
pub struct ReadyStatus {
    ready: bool,
    /// Why not, as a fixed code.
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
}

/// Whether the instance can do its work: the engine admits requests and
/// answers a handshake, the API is not draining, and the host's check
/// passes. `503` with a reason otherwise. Needs no credential and reveals
/// nothing beyond the fixed reason.
pub async fn ready(State(app): State<AppState>) -> (StatusCode, Json<ReadyStatus>) {
    let reason = not_ready(&app).await;
    let status = match reason {
        None => StatusCode::OK,
        Some(_) => StatusCode::SERVICE_UNAVAILABLE,
    };
    let ready = reason.is_none();
    (status, Json(ReadyStatus { ready, reason }))
}

async fn not_ready(app: &AppState) -> Option<&'static str> {
    if app.shutdown.is_cancelled() {
        return Some("shutting_down");
    }
    match app.engine.readiness() {
        Readiness::Ready => {}
        Readiness::Starting => return Some("starting"),
        Readiness::Failed => return Some("engine_failed"),
        Readiness::ShuttingDown | Readiness::Stopped => return Some("shutting_down"),
    }
    tokio::time::timeout(app.readiness_timeout, checks(app))
        .await
        .unwrap_or(Some("check_timed_out"))
}

/// The checks that can take time: a handshake and the host's check.
async fn checks(app: &AppState) -> Option<&'static str> {
    // The engine answers, rather than only saying it would.
    if app.engine.hello().await.is_err() {
        return Some("engine_unavailable");
    }
    let check = app.readiness_check.clone()?;
    // One check at a time. A check stuck in a filesystem call keeps its
    // permit, so later requests time out waiting for it instead of each
    // tying up another blocking thread.
    let permit = app
        .readiness_running
        .clone()
        .acquire_owned()
        .await
        .expect("the semaphore is never closed");
    tokio::task::spawn_blocking(move || {
        let _running = permit;
        check()
    })
    .await
    .unwrap_or(Err("check_failed"))
    .err()
}

/// Submits a batch of URLs.
pub async fn submit(
    State(app): State<AppState>,
    Extension(grant): Extension<Grant>,
    JsonBody(request): JsonBody<SubmitRequest>,
) -> ApiResult<CommandResult> {
    execute(&app, &grant, request.into_envelope()?).await
}

/// Pauses, resumes, cancels, renames, or removes a job at the revision the
/// client saw.
pub async fn job_command(
    State(app): State<AppState>,
    Extension(grant): Extension<Grant>,
    Path(id): Path<String>,
    JsonBody(request): JsonBody<JobCommandRequest>,
) -> ApiResult<CommandResult> {
    execute(&app, &grant, request.into_envelope(job_id(&id)?)?).await
}

/// The default destination and the roots a client may choose. Paths are
/// where the roots are on the server.
pub async fn settings(
    State(app): State<AppState>,
    Extension(grant): Extension<Grant>,
) -> ApiResult<QueryResult> {
    Ok(Json(app.engine.query(Query::Settings {}, &grant.0).await?))
}

/// Chooses the default destination at the settings revision the client
/// saw.
pub async fn update_settings(
    State(app): State<AppState>,
    Extension(grant): Extension<Grant>,
    JsonBody(request): JsonBody<UpdateSettingsRequest>,
) -> ApiResult<CommandResult> {
    execute(&app, &grant, request.into_envelope()).await
}

/// The query string of `GET /v1/folders`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FolderParams {
    /// An absolute path; the top of the filesystem if absent.
    path: Option<String>,
}

/// The folders inside a folder on the server, for the owner to choose one
/// to add as a destination. Needs the `control` permission.
pub async fn folders(
    State(app): State<AppState>,
    Extension(grant): Extension<Grant>,
    params: Result<UrlQuery<FolderParams>, axum::extract::rejection::QueryRejection>,
) -> ApiResult<FolderListing> {
    let UrlQuery(params) =
        params.map_err(|_| ApiError::new(ErrorCode::InvalidRequest, "invalid query string"))?;
    let path = params.path.map(std::path::PathBuf::from);
    Ok(Json(app.engine.browse_folders(path, &grant.0).await?))
}

/// Permits a folder on the server as a destination. Needs the `control`
/// permission.
pub async fn add_root(
    State(app): State<AppState>,
    Extension(grant): Extension<Grant>,
    JsonBody(request): JsonBody<AddRootRequest>,
) -> ApiResult<RootSummary> {
    let path = std::path::PathBuf::from(request.path);
    Ok(Json(app.engine.register_root(path, &grant.0).await?))
}

/// Stops offering a folder the user added as a destination; its files
/// stay where they are. Needs the `control` permission.
pub async fn remove_root(
    State(app): State<AppState>,
    Extension(grant): Extension<Grant>,
    JsonBody(request): JsonBody<RemoveRootRequest>,
) -> ApiResult<CommandResult> {
    execute(&app, &grant, request.into_envelope()).await
}

async fn execute(
    app: &AppState,
    grant: &Grant,
    envelope: CommandEnvelope,
) -> ApiResult<CommandResult> {
    Ok(Json(app.engine.execute(envelope, &grant.0).await?))
}

/// The query string of `GET /v1/jobs`. Values are parsed here rather than
/// by the URL decoder, so a bad value gets its specific error code.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListParams {
    after: Option<String>,
    page_size: Option<String>,
    /// Comma-separated stages.
    stage: Option<String>,
    package: Option<String>,
}

/// A page of jobs.
pub async fn list_jobs(
    State(app): State<AppState>,
    Extension(grant): Extension<Grant>,
    params: Result<UrlQuery<ListParams>, axum::extract::rejection::QueryRejection>,
) -> ApiResult<QueryResult> {
    let UrlQuery(params) =
        params.map_err(|_| ApiError::new(ErrorCode::InvalidRequest, "invalid query string"))?;
    let query = Query::ListJobs {
        filter: job_filter(params.stage.as_deref(), params.package.as_deref())?,
        after: params.after.as_deref().map(job_id).transpose()?,
        page_size: page_size(params.page_size.as_deref())?,
    };
    Ok(Json(app.engine.query(query, &grant.0).await?))
}

/// A `page_size` parameter, or the default.
fn page_size(raw: Option<&str>) -> Result<PageSize, ApiError> {
    let Some(raw) = raw else {
        return Ok(PageSize::DEFAULT);
    };
    let n = raw
        .parse::<u16>()
        .map_err(|_| ApiError::new(ErrorCode::InvalidRequest, "invalid page_size"))?;
    Ok(PageSize::try_from(n)?)
}

/// The query string of `GET /v1/job-counts`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CountParams {
    /// Comma-separated stages.
    stage: Option<String>,
    package: Option<String>,
}

/// How many jobs are in each stage.
pub async fn count_jobs(
    State(app): State<AppState>,
    Extension(grant): Extension<Grant>,
    params: Result<UrlQuery<CountParams>, axum::extract::rejection::QueryRejection>,
) -> ApiResult<QueryResult> {
    let UrlQuery(params) =
        params.map_err(|_| ApiError::new(ErrorCode::InvalidRequest, "invalid query string"))?;
    let query = Query::CountJobs {
        filter: job_filter(params.stage.as_deref(), params.package.as_deref())?,
    };
    Ok(Json(app.engine.query(query, &grant.0).await?))
}

/// The filter named by `stage` (comma-separated) and `package` parameters.
fn job_filter(stage: Option<&str>, package: Option<&str>) -> Result<JobFilter, ApiError> {
    let stages = stage
        .iter()
        .flat_map(|s| s.split(','))
        .map(|s| {
            JobStage::from_str(s)
                .map_err(|_| ApiError::new(ErrorCode::InvalidRequest, "unknown stage"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let package = package
        .map(|p| {
            PackageId::from_str(p)
                .map_err(|_| ApiError::new(ErrorCode::InvalidRequest, "invalid package"))
        })
        .transpose()?;
    Ok(JobFilter { stages, package })
}

/// One job.
pub async fn inspect_job(
    State(app): State<AppState>,
    Extension(grant): Extension<Grant>,
    Path(id): Path<String>,
) -> ApiResult<QueryResult> {
    let query = Query::InspectJob { job: job_id(&id)? };
    Ok(Json(app.engine.query(query, &grant.0).await?))
}

/// The link a job was submitted with. Needs the `sources` permission.
pub async fn job_source(
    State(app): State<AppState>,
    Extension(grant): Extension<Grant>,
    Path(id): Path<String>,
) -> ApiResult<QueryResult> {
    let query = Query::JobSource { job: job_id(&id)? };
    Ok(Json(app.engine.query(query, &grant.0).await?))
}

/// Collects links for review without starting them.
pub async fn collect_links(
    State(app): State<AppState>,
    Extension(grant): Extension<Grant>,
    JsonBody(request): JsonBody<CollectRequest>,
) -> ApiResult<CommandResult> {
    execute(&app, &grant, request.into_envelope()?).await
}

/// Starts, removes, or moves collected links at the revisions the client
/// saw.
pub async fn links_command(
    State(app): State<AppState>,
    Extension(grant): Extension<Grant>,
    JsonBody(request): JsonBody<LinksCommandRequest>,
) -> ApiResult<CommandResult> {
    execute(&app, &grant, request.into_envelope()?).await
}

/// The query string of `GET /v1/staging/links`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkListParams {
    after: Option<String>,
    page_size: Option<String>,
}

/// A page of collected links.
pub async fn list_links(
    State(app): State<AppState>,
    Extension(grant): Extension<Grant>,
    params: Result<UrlQuery<LinkListParams>, axum::extract::rejection::QueryRejection>,
) -> ApiResult<QueryResult> {
    let UrlQuery(params) =
        params.map_err(|_| ApiError::new(ErrorCode::InvalidRequest, "invalid query string"))?;
    let query = Query::ListLinks {
        after: params.after.as_deref().map(link_id).transpose()?,
        page_size: page_size(params.page_size.as_deref())?,
    };
    Ok(Json(app.engine.query(query, &grant.0).await?))
}

/// A collected link as it was entered. Needs the `sources` permission.
pub async fn link_source(
    State(app): State<AppState>,
    Extension(grant): Extension<Grant>,
    Path(id): Path<String>,
) -> ApiResult<QueryResult> {
    let query = Query::LinkSource {
        link: link_id(&id)?,
    };
    Ok(Json(app.engine.query(query, &grant.0).await?))
}

/// The query string of `GET /v1/events`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventParams {
    /// Replay events after this cursor. `Last-Event-ID` takes precedence,
    /// so an automatic reconnect continues where the stream broke off.
    after: Option<String>,
}

/// The event stream (architecture brief sections 4.7 and 7.8).
///
/// Each item is an SSE event named after its [`StreamItem`] type, with the
/// item's JSON as data. Durable events carry their cursor as the event ID,
/// `<instance>:<position>`, so a reconnecting client sends it back as
/// `Last-Event-ID` and gets only what it missed. A `resync` event ends the
/// stream: the client re-queries and subscribes from the new snapshot.
///
/// A stream opened with a browser session ends when the session does:
/// on sign-out, expiry, or replacement.
///
/// The stream is pulled only as fast as the client reads, and the engine's
/// bounded ring decides when a slow client must resynchronize, so no
/// client grows memory. Closing the stream affects nothing but the stream.
pub async fn events(
    State(app): State<AppState>,
    Extension(grant): Extension<Grant>,
    session: Option<Extension<SessionEnd>>,
    headers: HeaderMap,
    params: Result<UrlQuery<EventParams>, axum::extract::rejection::QueryRejection>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let UrlQuery(params) =
        params.map_err(|_| ApiError::new(ErrorCode::InvalidRequest, "invalid query string"))?;
    let last_event_id = headers
        .get("last-event-id")
        .map(|v| {
            v.to_str()
                .map_err(|_| ApiError::new(ErrorCode::InvalidRequest, "invalid Last-Event-ID"))
        })
        .transpose()?
        .filter(|v| !v.is_empty());
    let after = last_event_id
        .or(params.after.as_deref())
        .map(parse_cursor)
        .transpose()?;
    let permit = app
        .streams
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::new(ErrorCode::Overloaded, "too many open event streams"))?;
    let subscription = app.engine.subscribe(Subscribe { after }, &grant.0).await?;
    let stream = futures_util::stream::unfold(
        (subscription, permit),
        |(mut subscription, permit)| async move {
            let item = subscription.next().await?;
            Some((Ok(sse_event(&item)), (subscription, permit)))
        },
    )
    .take_until(futures_util::future::select(
        Box::pin(app.shutdown.cancelled_owned()),
        Box::pin(async move {
            match session {
                Some(Extension(end)) => end.ended().await,
                None => std::future::pending().await,
            }
        }),
    ));
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(app.keep_alive)))
}

fn sse_event(item: &StreamItem) -> Event {
    let data = serde_json::to_string(item).expect("stream items serialize");
    let event = Event::default().data(data);
    match item {
        StreamItem::Hello(_) => event.event("hello"),
        StreamItem::Event(e) => event.event("event").id(cursor_id(e.cursor)),
        StreamItem::Progress(_) => event.event("progress"),
        StreamItem::Resync { .. } => event.event("resync"),
    }
}

/// A cursor as an SSE event ID.
pub fn cursor_id(cursor: EventCursor) -> String {
    format!("{}:{}", cursor.instance, cursor.position)
}

/// Parses an event ID from [`cursor_id`].
fn parse_cursor(text: &str) -> Result<EventCursor, ApiError> {
    let invalid = || ApiError::new(ErrorCode::InvalidRequest, "invalid event cursor");
    let (instance, position) = text.split_once(':').ok_or_else(invalid)?;
    let instance = serde_json::from_value::<InstanceId>(instance.into()).map_err(|_| invalid())?;
    let position = position.parse().map_err(|_| invalid())?;
    Ok(EventCursor { instance, position })
}

fn job_id(text: &str) -> Result<JobId, ApiError> {
    JobId::from_str(text).map_err(|_| ApiError::new(ErrorCode::InvalidRequest, "invalid job ID"))
}

fn link_id(text: &str) -> Result<SourceLinkId, ApiError> {
    SourceLinkId::from_str(text)
        .map_err(|_| ApiError::new(ErrorCode::InvalidRequest, "invalid link ID"))
}

/// A JSON request body, read within the body size limit and the body
/// deadline. Without the deadline, a client that sends headers and then
/// stalls its body would hold a connection for as long as it likes.
pub struct JsonBody<T>(pub T);

impl<T: DeserializeOwned> FromRequest<AppState> for JsonBody<T> {
    type Rejection = ApiError;

    async fn from_request(request: Request, app: &AppState) -> Result<Self, ApiError> {
        let bytes = tokio::time::timeout(app.body_timeout, Bytes::from_request(request, app))
            .await
            .map_err(|_| ApiError::body_timeout())??;
        serde_json::from_slice(&bytes)
            .map(Self)
            .map_err(|e| ApiError::bad_json(&e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_round_trip_through_event_ids() {
        let cursor = EventCursor {
            instance: InstanceId::generate(),
            position: 42,
        };
        assert_eq!(parse_cursor(&cursor_id(cursor)).unwrap(), cursor);
        let id = cursor_id(cursor);
        for bad in ["", "42", ":42", "nope:1", &id.replace(":42", ":x")] {
            assert!(parse_cursor(bad).is_err(), "{bad}");
        }
    }
}
