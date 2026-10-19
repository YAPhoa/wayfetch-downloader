//! The HTTP client behind the engine's [`Transport`], and the broker that
//! holds prepared requests and open responses for transfer leases
//! (architecture brief sections 6.5, 7.5, and 8.2; ADR-09, ADR-14, ADR-15).
//!
//! Policies the client is configured with, rather than library defaults:
//!
//! * TLS through rustls with the platform's roots plus any configured
//!   extra roots; verification cannot be switched off.
//! * HTTP/1.1 only, no proxy, no cookies, no `Referer`, no automatic
//!   retries, and no decompression: the request asks for `identity` and an
//!   encoded answer is refused.
//! * Redirects are followed here, one hop at a time, within a hop budget.
//!   Every target passes the [`EgressPolicy`]; a hop to another origin drops
//!   credentials and never carries a request body; HTTPS never downgrades
//!   to HTTP; a hop that would resend a body or an unsafe method, or
//!   request a URL already requested, is followed only for a request that
//!   may be repeated.
//! * Names resolve through the policy, so the connector only ever sees
//!   permitted addresses. A request for a link found on a web page
//!   ([`Reach::Public`]) uses a client of its own that reaches public
//!   addresses only, whatever local networks the operator allows.
//!
//! The broker never sends a request twice unless the transfer's lease
//! authorizes the repetition, and hands out an open response's body once.
//! A failure after a non-replayable request may have reached the server is
//! reported as an ambiguous outcome, not a timeout (T11, T12).

use std::collections::HashMap;
use std::error::Error as StdError;
use std::io;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use reqwest::header::{self, HeaderMap, HeaderValue};
use reqwest::{Method, StatusCode};
use rustls::pki_types::CertificateDer;
use url::Url;
use wayfetch_engine::domain::failure::FailureCode;
use wayfetch_engine::domain::time::UtcTimestamp;
use wayfetch_engine::transfer::lease::{
    LeaseSource, Replay, ReplayPolicy, Representation, RequestPurpose, RequestRef, ResponseRef,
    Transfer,
};
use wayfetch_engine::transfer::transport::{
    OpenRequest, Reach, Response, Transport, TransportError,
};
use wayfetch_engine::transfer::writer::BoxFuture;

use crate::body::Body;
use crate::head::{self, ResponseHead, error};
use crate::policy::{EgressPolicy, PolicyResolver, Refused};

/// Client settings.
#[derive(Debug, Clone)]
pub struct HttpConfig {
    /// Destinations requests may reach.
    pub egress: EgressPolicy,
    /// Redirects followed for one request before it fails.
    pub max_redirects: u8,
    /// Certificate authorities trusted in addition to the platform's, for
    /// example a NAS operator's own CA.
    pub extra_roots: Vec<CertificateDer<'static>>,
    /// The `User-Agent` sent with every request.
    pub user_agent: String,
    /// Prepared requests the broker holds at once.
    pub max_prepared: NonZeroUsize,
    /// Open responses the broker holds at once, each with its connection.
    pub max_open_responses: NonZeroUsize,
    /// Idle connections kept per host.
    pub idle_per_host: usize,
    /// How long an idle connection is kept.
    pub idle_timeout: Duration,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            egress: EgressPolicy::default(),
            max_redirects: 10,
            extra_roots: Vec::new(),
            user_agent: concat!("wayfetch/", env!("CARGO_PKG_VERSION")).into(),
            max_prepared: NonZeroUsize::new(64).expect("nonzero"),
            max_open_responses: NonZeroUsize::new(8).expect("nonzero"),
            idle_per_host: 2,
            idle_timeout: Duration::from_secs(90),
        }
    }
}

/// The client could not be configured.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    /// The TLS configuration was refused, for example an invalid root.
    #[error("TLS configuration: {0}")]
    Tls(#[from] rustls::Error),
    /// The HTTP client could not be built.
    #[error("HTTP client: {0}")]
    Client(#[from] reqwest::Error),
}

/// A request an adapter prepared, for example a form submission. Its
/// headers and body may carry secrets; they stay in the broker, and a lease
/// refers to the request by [`RequestRef`] (section 6.5).
#[derive(Clone)]
pub struct PreparedRequest {
    /// The method.
    pub method: Method,
    /// The URL.
    pub url: Url,
    /// Headers to send. Credentials among them (`Authorization`, `Cookie`,
    /// or any value marked sensitive) are dropped on a redirect to another
    /// origin. Framing and range headers are the broker's and are replaced.
    pub headers: HeaderMap,
    /// The body, if any. It is never sent to another origin.
    pub body: Option<Bytes>,
}

impl std::fmt::Debug for PreparedRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The URL's path and query, headers, and body may be secret.
        f.debug_struct("PreparedRequest")
            .field("method", &self.method)
            .field("origin", &self.url.origin().ascii_serialization())
            .finish_non_exhaustive()
    }
}

/// What to send for a leased transfer.
#[derive(Debug, Clone)]
pub struct LeaseRequest<'a> {
    /// Why this is not the first request: `None` sends the prepared request
    /// for the first time, or takes the open response. Repeating a request
    /// that was already sent without a purpose counts as a retry.
    pub purpose: Option<RequestPurpose>,
    /// The byte interval to request, as in [`OpenRequest::range`].
    pub range: Option<(u64, Option<u64>)>,
    /// The representation a range must still belong to.
    pub validators: Option<&'a Representation>,
    /// Deadline for establishing a connection.
    pub connect_timeout: Duration,
    /// Largest chunk the body may yield at once.
    pub read_buffer: usize,
}

/// Why the broker produced no response.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BrokerError {
    /// The request was sent and failed.
    #[error(transparent)]
    Transport(#[from] TransportError),
    /// The lease does not allow sending the request again, or the open
    /// response was already handed out; nothing was sent.
    #[error("the lease does not allow this request ({0:?})")]
    Refused(Replay),
    /// The broker holds no such request or response.
    #[error("unknown request or response")]
    Unknown,
    /// The lease's replay policy is not the one the request was prepared
    /// with; nothing was sent.
    #[error("the lease's replay policy {lease:?} differs from the prepared {prepared:?}")]
    PolicyMismatch {
        /// The policy the request was prepared with.
        prepared: ReplayPolicy,
        /// The policy the lease's offer names.
        lease: ReplayPolicy,
    },
    /// The broker already holds as many requests or responses as allowed.
    #[error("the broker is full")]
    Full,
}

struct Prepared {
    request: PreparedRequest,
    replay: ReplayPolicy,
    /// Whether the request was handed to the client at least once. Set
    /// before sending, so a send that was cancelled or failed still counts.
    sent: bool,
}

#[derive(Default)]
struct Broker {
    next: u64,
    requests: HashMap<u64, Prepared>,
    responses: HashMap<u64, Response>,
    /// Response slots held by submissions still in flight.
    reserved: usize,
}

impl Broker {
    fn id(&mut self) -> u64 {
        self.next += 1;
        self.next
    }
}

/// A response slot held while its request is in flight, so concurrent
/// submissions cannot hold more open responses than allowed. Released when
/// dropped, including when the submission is cancelled.
struct Slot<'a>(&'a Mutex<Broker>);

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.0.lock().expect("unpoisoned").reserved -= 1;
    }
}

/// One request as sent, hop by hop.
pub(crate) struct Outgoing<'a> {
    pub(crate) method: Method,
    pub(crate) url: Url,
    pub(crate) headers: HeaderMap,
    pub(crate) body: Option<Bytes>,
    pub(crate) range: Option<(u64, Option<u64>)>,
    pub(crate) validators: Option<&'a Representation>,
    pub(crate) connect_timeout: Duration,
    /// Which addresses it may reach, redirects included.
    pub(crate) reach: Reach,
    /// Whether sending it again would repeat no action: decides whether a
    /// failure after it may have reached the server is a plain timeout or
    /// an ambiguous outcome.
    pub(crate) replay_safe: bool,
}

/// An egress policy, and the client whose resolver applies it.
struct Egress {
    policy: Arc<EgressPolicy>,
    /// The client for the last connect timeout asked for; the engine uses
    /// one, so the pool is normally kept.
    client: Mutex<Option<(Duration, reqwest::Client)>>,
}

impl Egress {
    fn new(policy: EgressPolicy) -> Self {
        Self {
            policy: Arc::new(policy),
            client: Mutex::new(None),
        }
    }
}

/// An HTTP(S) transport for the engine and a request broker for leases.
pub struct HttpTransport {
    config: HttpConfig,
    /// The operator's policy, for links the person adds.
    allowed: Egress,
    /// Public addresses only, for links found on pages; `None` when the
    /// operator's policy allows no more than that anyway.
    public: Option<Egress>,
    tls: rustls::ClientConfig,
    broker: Mutex<Broker>,
}

impl std::fmt::Debug for HttpTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpTransport")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// Connect timeout used to check the configuration when building.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

impl HttpTransport {
    /// A transport with `config`.
    pub fn new(config: HttpConfig) -> Result<Self, BuildError> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier = rustls_platform_verifier::Verifier::new_with_extra_roots(
            config.extra_roots.iter().cloned(),
            provider.clone(),
        )?;
        // A custom verifier is how rustls takes the platform verifier; it
        // verifies certificates fully.
        let mut tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_no_client_auth();
        tls.alpn_protocols = vec![b"http/1.1".to_vec()];
        let public = EgressPolicy::default();
        let transport = Self {
            allowed: Egress::new(config.egress.clone()),
            public: (config.egress != public).then(|| Egress::new(public)),
            config,
            tls,
            broker: Mutex::default(),
        };
        // Building the clients checks the configuration.
        for egress in [Some(&transport.allowed), transport.public.as_ref()]
            .into_iter()
            .flatten()
        {
            let client = transport.build_client(&egress.policy, DEFAULT_CONNECT_TIMEOUT)?;
            *egress.client.lock().expect("unpoisoned") = Some((DEFAULT_CONNECT_TIMEOUT, client));
        }
        Ok(transport)
    }

    /// The policy and client for requests with `reach`.
    fn egress(&self, reach: Reach) -> &Egress {
        match reach {
            Reach::Allowed => &self.allowed,
            Reach::Public => self.public.as_ref().unwrap_or(&self.allowed),
        }
    }

    /// The policy requests with `reach` are checked against.
    pub(crate) fn policy(&self, reach: Reach) -> &EgressPolicy {
        &self.egress(reach).policy
    }

    fn build_client(
        &self,
        policy: &Arc<EgressPolicy>,
        connect_timeout: Duration,
    ) -> Result<reqwest::Client, reqwest::Error> {
        reqwest::Client::builder()
            .tls_backend_preconfigured(self.tls.clone())
            .http1_only()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .referer(false)
            .dns_resolver(PolicyResolver(policy.clone()))
            .connect_timeout(connect_timeout)
            .pool_max_idle_per_host(self.config.idle_per_host)
            .pool_idle_timeout(self.config.idle_timeout)
            .user_agent(&self.config.user_agent)
            .build()
    }

    pub(crate) fn client(
        &self,
        reach: Reach,
        connect_timeout: Duration,
    ) -> Result<reqwest::Client, TransportError> {
        let egress = self.egress(reach);
        let mut slot = egress.client.lock().expect("unpoisoned");
        if let Some((timeout, client)) = &*slot
            && *timeout == connect_timeout
        {
            return Ok(client.clone());
        }
        let client = self
            .build_client(&egress.policy, connect_timeout)
            .map_err(|e| {
                error(
                    FailureCode::UnsupportedCapability,
                    format!("HTTP client: {}", e.without_url()),
                )
            })?;
        *slot = Some((connect_timeout, client.clone()));
        Ok(client)
    }

    /// Holds `request` for leases; its secrets never leave the broker.
    /// `replay` says whether it may be repeated, as the lease's offer will.
    pub fn prepare(
        &self,
        request: PreparedRequest,
        replay: ReplayPolicy,
    ) -> Result<RequestRef, BrokerError> {
        let mut broker = self.broker.lock().expect("unpoisoned");
        if broker.requests.len() >= self.config.max_prepared.get() {
            return Err(BrokerError::Full);
        }
        let id = broker.id();
        broker.requests.insert(
            id,
            Prepared {
                request,
                replay,
                sent: false,
            },
        );
        Ok(RequestRef(id))
    }

    /// Drops a prepared request.
    pub fn forget(&self, request: RequestRef) {
        self.broker
            .lock()
            .expect("unpoisoned")
            .requests
            .remove(&request.0);
    }

    /// Drops an open response that will not be transferred, closing its
    /// connection.
    pub fn discard(&self, response: ResponseRef) {
        self.broker
            .lock()
            .expect("unpoisoned")
            .responses
            .remove(&response.0);
    }

    /// Sends a prepared request once and holds its response open, so a
    /// download the request returned directly can be promoted to a lease
    /// ([`LeaseSource::OpenResponse`]) without sending it again. Returns
    /// the head for the caller to classify; the body stays unread.
    pub async fn submit(
        &self,
        request: RequestRef,
        connect_timeout: Duration,
        read_buffer: usize,
    ) -> Result<(ResponseRef, ResponseHead), BrokerError> {
        let (prepared, replay) = {
            let mut broker = self.broker.lock().expect("unpoisoned");
            if broker.responses.len() + broker.reserved >= self.config.max_open_responses.get() {
                return Err(BrokerError::Full);
            }
            let entry = broker
                .requests
                .get_mut(&request.0)
                .ok_or(BrokerError::Unknown)?;
            if entry.sent {
                return Err(BrokerError::Refused(Replay::Reresolve));
            }
            entry.sent = true;
            let prepared = (entry.request.clone(), entry.replay);
            broker.reserved += 1;
            prepared
        };
        let slot = Slot(&self.broker);
        let response = self
            .send_prepared(prepared, replay, None, None, connect_timeout, read_buffer)
            .await?;
        let head = ResponseHead {
            framing: response.framing,
            representation: response.representation.clone(),
            filename: response.filename.clone(),
        };
        let id = {
            let mut broker = self.broker.lock().expect("unpoisoned");
            let id = broker.id();
            broker.responses.insert(id, response);
            id
        };
        drop(slot);
        Ok((ResponseRef(id), head))
    }

    /// Opens a leased transfer's source. The first request sends the
    /// prepared request or takes the open response; every later one must be
    /// authorized by the lease ([`Transfer::authorize`]), so a
    /// non-replayable request is never sent twice and an open response is
    /// never cloned (section 6.5). A lease must name the replay policy its
    /// request was prepared with, so it cannot weaken it.
    pub async fn open_lease(
        &self,
        transfer: &Transfer,
        request: &LeaseRequest<'_>,
    ) -> Result<Response, BrokerError> {
        let now = UtcTimestamp::from_system_time(SystemTime::now());
        let authorize = |purpose| match transfer.authorize(purpose, now) {
            Replay::Allowed => Ok(()),
            other => Err(BrokerError::Refused(other)),
        };
        let (prepared, replay) = {
            let mut broker = self.broker.lock().expect("unpoisoned");
            if let LeaseSource::PreparedRequest(id) = transfer.offer().source {
                let entry = broker.requests.get(&id.0).ok_or(BrokerError::Unknown)?;
                let lease = transfer.offer().replay;
                if entry.replay != lease {
                    return Err(BrokerError::PolicyMismatch {
                        prepared: entry.replay,
                        lease,
                    });
                }
            }
            if let Some(purpose) = request.purpose {
                authorize(purpose)?;
            }
            match transfer.offer().source {
                LeaseSource::OpenResponse(id) => {
                    return broker
                        .responses
                        .remove(&id.0)
                        .ok_or(BrokerError::Refused(Replay::Reresolve));
                }
                LeaseSource::PreparedRequest(id) => {
                    let entry = broker.requests.get_mut(&id.0).ok_or(BrokerError::Unknown)?;
                    if entry.sent && request.purpose.is_none() {
                        authorize(RequestPurpose::Retry)?;
                    }
                    entry.sent = true;
                    (entry.request.clone(), entry.replay)
                }
            }
        };
        Ok(self
            .send_prepared(
                prepared,
                replay,
                request.range,
                request.validators,
                request.connect_timeout,
                request.read_buffer,
            )
            .await?)
    }

    async fn send_prepared(
        &self,
        prepared: PreparedRequest,
        replay: ReplayPolicy,
        range: Option<(u64, Option<u64>)>,
        validators: Option<&Representation>,
        connect_timeout: Duration,
        read_buffer: usize,
    ) -> Result<Response, TransportError> {
        let replay_safe = replay == ReplayPolicy::Idempotent;
        self.fetch(
            Outgoing {
                method: prepared.method,
                url: prepared.url,
                headers: prepared.headers,
                body: prepared.body,
                range,
                validators,
                connect_timeout,
                // A provider's link is the provider's answer, not a page's.
                reach: Reach::Allowed,
                replay_safe,
            },
            read_buffer,
        )
        .await
    }

    /// Sends `out`, following redirects, and opens the final response.
    async fn fetch(
        &self,
        out: Outgoing<'_>,
        read_buffer: usize,
    ) -> Result<Response, TransportError> {
        let replay_safe = out.replay_safe;
        let (response, url) = self.send(out).await?;
        let status = response.status();
        if !matches!(
            status,
            StatusCode::OK | StatusCode::PARTIAL_CONTENT | StatusCode::RANGE_NOT_SATISFIABLE
        ) {
            return Err(head::status_failure(status, replay_safe));
        }
        let head = head::read(status, response.headers(), &url)?;
        Ok(Response {
            framing: head.framing,
            representation: head.representation,
            filename: head.filename,
            body: Box::new(Body::new(response, read_buffer)),
        })
    }

    /// Sends `out` and follows redirects within the hop budget, checking
    /// every target. Returns the final response and its URL.
    pub(crate) async fn send(
        &self,
        mut out: Outgoing<'_>,
    ) -> Result<(reqwest::Response, Url), TransportError> {
        let client = self.client(out.reach, out.connect_timeout)?;
        let policy = self.policy(out.reach);
        let mut headers = caller_headers(std::mem::take(&mut out.headers));
        let mut hops = 0;
        // URLs sent so far, without fragments, which are never sent.
        let mut sent: Vec<Url> = Vec::new();
        loop {
            policy.check(&out.url).map_err(refused)?;
            let mut request = client
                .request(out.method.clone(), out.url.clone())
                .headers(headers.clone())
                .header(header::ACCEPT_ENCODING, "identity");
            if let Some(range) = out.range {
                request = request.header(header::RANGE, range_header(range)?);
                if let Some(value) = out.validators.and_then(if_range) {
                    request = request.header(header::IF_RANGE, value);
                }
            }
            if let Some(body) = &out.body {
                request = request.body(body.clone());
            }
            let mut target = out.url.clone();
            target.set_fragment(None);
            sent.push(target);
            let response = request
                .send()
                .await
                .map_err(|e| send_failure(e, out.replay_safe))?;
            let status = response.status();
            if !matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308) {
                return Ok((response, out.url));
            }
            let Some(location) = response.headers().get(header::LOCATION) else {
                return Ok((response, out.url));
            };
            if hops == self.config.max_redirects {
                return Err(refused(Refused(format!(
                    "more than {} redirects",
                    self.config.max_redirects
                ))));
            }
            hops += 1;
            let next = location
                .to_str()
                .ok()
                .and_then(|l| out.url.join(l).ok())
                .ok_or_else(|| refused(Refused("invalid redirect location".into())))?;
            if out.url.scheme() == "https" && next.scheme() == "http" {
                return Err(refused(Refused("redirect from HTTPS to HTTP".into())));
            }
            let cross_origin = next.origin() != out.url.origin();
            // A 303, or a 301 or 302 after a POST, continues as a GET
            // without the body (RFC 9110, section 15.4).
            if status == StatusCode::SEE_OTHER && out.method != Method::HEAD
                || matches!(status.as_u16(), 301 | 302) && out.method == Method::POST
            {
                out.method = Method::GET;
                out.body = None;
                headers.remove(header::CONTENT_TYPE);
            }
            if cross_origin {
                if out.body.is_some() {
                    return Err(refused(Refused(
                        "redirect would send the request body to another origin".into(),
                    )));
                }
                headers = without_credentials(headers);
            }
            // Following the redirect may send the request again. A body, an
            // unsafe method, or any method to a URL already requested in
            // this chain is sent only if the request may be repeated: the
            // first one reached the server, so for any other the outcome is
            // unknown and it is never resent (section 6.5, T11, T12).
            let mut target = next.clone();
            target.set_fragment(None);
            if !out.replay_safe
                && (out.body.is_some() || !out.method.is_safe() || sent.contains(&target))
            {
                return Err(error(
                    FailureCode::OperationAmbiguous,
                    format!("HTTP {status} would send a non-replayable request again"),
                ));
            }
            out.url = next;
        }
    }
}

impl Transport for HttpTransport {
    /// A direct link: a plain GET, which may be repeated.
    fn open<'a>(
        &'a self,
        request: &'a OpenRequest,
    ) -> BoxFuture<'a, Result<Response, TransportError>> {
        let mut headers = HeaderMap::new();
        // A link made for one client's `User-Agent` is sent with it; an
        // unusable value is dropped rather than sent altered.
        if let Some(agent) = request
            .user_agent
            .as_deref()
            .and_then(|a| HeaderValue::from_str(a).ok())
        {
            headers.insert(header::USER_AGENT, agent);
        }
        Box::pin(self.fetch(
            Outgoing {
                method: Method::GET,
                url: request.url.clone(),
                headers,
                body: None,
                range: request.range,
                validators: request.validators.as_ref(),
                connect_timeout: request.connect_timeout,
                reach: request.reach,
                replay_safe: true,
            },
            request.read_buffer,
        ))
    }
}

fn refused(refused: Refused) -> TransportError {
    error(FailureCode::UnsupportedCapability, refused.0)
}

/// Headers the broker sets itself are removed from a caller's.
fn caller_headers(mut headers: HeaderMap) -> HeaderMap {
    for name in [
        header::HOST,
        header::CONTENT_LENGTH,
        header::TRANSFER_ENCODING,
        header::ACCEPT_ENCODING,
        header::RANGE,
        header::IF_RANGE,
        header::REFERER,
    ] {
        headers.remove(name);
    }
    headers
}

/// Headers safe to send to another origin: no credentials.
fn without_credentials(headers: HeaderMap) -> HeaderMap {
    let mut kept = HeaderMap::with_capacity(headers.len());
    for (name, value) in &headers {
        let credential = value.is_sensitive()
            || name == header::AUTHORIZATION
            || name == header::PROXY_AUTHORIZATION
            || name == header::COOKIE;
        if !credential {
            kept.append(name.clone(), value.clone());
        }
    }
    kept
}

fn range_header((start, end): (u64, Option<u64>)) -> Result<String, TransportError> {
    match end {
        None => Ok(format!("bytes={start}-")),
        Some(end) if end > start => Ok(format!("bytes={start}-{}", end - 1)),
        Some(_) => Err(error(FailureCode::IntegrityMismatch, "empty byte range")),
    }
}

/// The `If-Range` validator for a range of `representation`: its strong
/// entity tag, else its modification time if that identifies it.
fn if_range(representation: &Representation) -> Option<HeaderValue> {
    if let Some(etag) = &representation.strong_etag {
        return HeaderValue::from_str(etag).ok();
    }
    if !representation.is_trustworthy() {
        return None;
    }
    let millis = u64::try_from(representation.last_modified?.unix_millis()).ok()?;
    let at = UNIX_EPOCH + Duration::from_millis(millis);
    HeaderValue::from_str(&httpdate::fmt_http_date(at)).ok()
}

/// Classifies a request that produced no response. Nothing reached the
/// server if the connection failed; after that, a non-replayable request's
/// outcome is unknown (T12).
fn send_failure(err: reqwest::Error, replay_safe: bool) -> TransportError {
    // The URL may carry a signed token.
    let err = err.without_url();
    if let Some(refusal) = find::<Refused>(&err) {
        return refused(refusal.clone());
    }
    if let Some(tls) = find::<rustls::Error>(&err) {
        return error(
            FailureCode::UnsupportedCapability,
            format!("TLS verification failed: {tls}"),
        );
    }
    let detail = match err.source() {
        Some(source) => format!("{err}: {source}"),
        None => err.to_string(),
    };
    if err.is_connect() || replay_safe {
        error(FailureCode::RequestTimeout, detail)
    } else {
        error(
            FailureCode::OperationAmbiguous,
            format!("no response after the request may have been sent: {detail}"),
        )
    }
}

/// The first `T` in `err`'s chain of causes, looking inside I/O errors,
/// which hide their inner error from `source`.
pub(crate) fn find<'e, T: StdError + 'static>(err: &'e (dyn StdError + 'static)) -> Option<&'e T> {
    let mut next = Some(err);
    while let Some(e) = next {
        if let Some(found) = e.downcast_ref::<T>() {
            return Some(found);
        }
        if let Some(inner) = e.downcast_ref::<io::Error>().and_then(io::Error::get_ref)
            && let Some(found) = find::<T>(inner)
        {
            return Some(found);
        }
        next = e.source();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_and_validators_are_spelled_exactly() {
        assert_eq!(range_header((10, None)).unwrap(), "bytes=10-");
        assert_eq!(range_header((10, Some(20))).unwrap(), "bytes=10-19");
        assert!(range_header((10, Some(10))).is_err());

        let mut r = Representation {
            length: Some(5),
            strong_etag: Some("\"v1\"".into()),
            last_modified: Some(UtcTimestamp::from_unix_millis(784_111_777_000)),
            resource: None,
        };
        assert_eq!(if_range(&r).unwrap(), "\"v1\"");
        r.strong_etag = None;
        assert_eq!(if_range(&r).unwrap(), "Sun, 06 Nov 1994 08:49:37 GMT");
        r.length = None;
        assert_eq!(if_range(&r), None);
    }

    #[test]
    fn credentials_never_cross_origins() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer x"));
        headers.insert(header::COOKIE, HeaderValue::from_static("session=x"));
        let mut token = HeaderValue::from_static("secret");
        token.set_sensitive(true);
        headers.insert("x-api-key", token);
        headers.insert(header::ACCEPT, HeaderValue::from_static("*/*"));
        headers.insert(header::RANGE, HeaderValue::from_static("bytes=0-"));
        let kept = without_credentials(caller_headers(headers));
        assert_eq!(kept.len(), 1);
        assert!(kept.contains_key(header::ACCEPT));
    }

    #[test]
    fn prepared_requests_do_not_print_secrets() {
        let request = PreparedRequest {
            method: Method::POST,
            url: Url::parse("https://api.example/unrestrict?token=secret").unwrap(),
            headers: HeaderMap::new(),
            body: Some(Bytes::from_static(b"password=secret")),
        };
        let printed = format!("{request:?}");
        assert!(!printed.contains("secret"), "{printed}");
    }
}
