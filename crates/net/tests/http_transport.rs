//! The HTTP transport and broker against a local server over real sockets
//! (architecture brief section 13.1). Brief tests exercised here at the
//! transport level: T01, T02, T06, T07, T08, T09, T10, T11, and T12, plus TLS
//! verification and the egress policy (section 8.2). The engine-level
//! scenarios are in `engine_over_http.rs`.

#[path = "../../engine/tests/support/mod.rs"]
mod engine_support;
mod support;

use std::num::{NonZeroU16, NonZeroUsize};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use engine_support::{KIB, body};
use support::{Raw, Resource, Route, Server, config_for};
use url::Url;
use wayfetch_engine::domain::failure::FailureCode;
use wayfetch_engine::domain::id::{AttemptId, JobId, ResourceId, RouteId};
use wayfetch_engine::domain::name::{HostName, ProviderId};
use wayfetch_engine::domain::source::Candidate;
use wayfetch_engine::domain::time::UtcTimestamp;
use wayfetch_engine::domain::version::Generation;
use wayfetch_engine::transfer::lease::{
    LeaseSource, Parallelism, Replay, ReplayPolicy, Representation, RequestPurpose, ResumePolicy,
    Transfer, TransferLease, TransferOffer,
};
use wayfetch_engine::transfer::range::Framing;
use wayfetch_engine::transfer::transport::{
    OpenRequest, Reach, Response, Transport, TransportError,
};
use wayfetch_net::header::{self, HeaderMap, HeaderValue};
use wayfetch_net::{BrokerError, HttpConfig, HttpTransport, LeaseRequest, Method, PreparedRequest};

const READ_BUFFER: usize = 4 * KIB;

fn transport(config: HttpConfig) -> HttpTransport {
    HttpTransport::new(config).unwrap()
}

fn request(url: &str) -> OpenRequest {
    OpenRequest {
        url: Url::parse(url).unwrap(),
        range: None,
        validators: None,
        connect_timeout: Duration::from_secs(5),
        read_buffer: READ_BUFFER,
        user_agent: None,
        reach: Reach::Allowed,
    }
}

async fn open(
    transport: &HttpTransport,
    request: &OpenRequest,
) -> Result<Response, TransportError> {
    transport.open(request).await
}

/// Reads a body to its end, checking that no chunk exceeds the read buffer.
async fn read_all(response: &mut Response) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(chunk) = response.body.next_chunk().await? {
        assert!(chunk.len() <= READ_BUFFER, "chunk of {}", chunk.len());
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

fn code(result: Result<Response, TransportError>) -> FailureCode {
    result.expect_err("a failure").code
}

#[tokio::test]
async fn files_download_over_https_and_empty_files_are_valid() {
    let server = Server::https().await;
    let data = body(100 * KIB + 3);
    server.put("/a.bin", Resource::new(data.clone()));
    server.put("/empty.bin", Resource::new(Bytes::new()));
    let transport = transport(config_for(&server));

    let mut response = open(&transport, &request(&server.url("/a.bin")))
        .await
        .unwrap();
    assert_eq!(
        response.framing,
        Framing::Full {
            length: Some(data.len() as u64)
        }
    );
    // The resource is identified without storing its URL.
    let resource = response.representation.resource.clone().unwrap();
    assert!(!resource.contains("a.bin"), "{resource}");
    assert_eq!(
        response.representation,
        Representation {
            length: Some(data.len() as u64),
            strong_etag: Some("\"v1\"".into()),
            last_modified: None,
            resource: Some(resource),
        }
    );
    assert_eq!(read_all(&mut response).await.unwrap(), data);

    // T01: an empty body with `Content-Length: 0` is a valid file.
    let mut empty = open(&transport, &request(&server.url("/empty.bin")))
        .await
        .unwrap();
    assert_eq!(empty.framing, Framing::Full { length: Some(0) });
    assert!(read_all(&mut empty).await.unwrap().is_empty());

    // The transfer asked for the identity encoding and sent no referrer.
    let logged = &server.log()[0];
    assert_eq!(logged.header("accept-encoding"), Some("identity"));
    assert_eq!(logged.header("referer"), None);
}

#[tokio::test]
async fn a_conditional_range_is_requested_and_framed() {
    let server = Server::http().await;
    let data = body(10 * KIB);
    server.put("/a.bin", Resource::new(data.clone()));
    let transport = transport(config_for(&server));
    let mut ranged = request(&server.url("/a.bin"));
    ranged.range = Some((4 * KIB as u64, None));
    ranged.validators = Some(Representation {
        length: Some(data.len() as u64),
        strong_etag: Some("\"v1\"".into()),
        last_modified: None,
        resource: None,
    });
    let mut response = open(&transport, &ranged).await.unwrap();
    // T02: the returned interval is the requested one.
    assert_eq!(
        response.framing,
        Framing::Partial {
            first: 4 * KIB as u64,
            last: data.len() as u64 - 1,
            complete: Some(data.len() as u64)
        }
    );
    assert_eq!(read_all(&mut response).await.unwrap(), data[4 * KIB..]);
    let logged = &server.log()[0];
    assert_eq!(logged.header("range"), Some("bytes=4096-"));
    assert_eq!(logged.header("if-range"), Some("\"v1\""));
}

#[tokio::test]
async fn encoded_bodies_are_refused_not_decoded() {
    let server = Server::http().await;
    server.put(
        "/a.tar",
        Resource::new(body(KIB)).header("Content-Encoding", "gzip"),
    );
    let transport = transport(config_for(&server));
    // T07: a body in another encoding would shift every byte offset.
    let err = open(&transport, &request(&server.url("/a.tar")))
        .await
        .unwrap_err();
    assert_eq!(err.code, FailureCode::UnsupportedCapability);
    assert!(err.message.contains("gzip"), "{}", err.message);
}

#[tokio::test]
async fn truncated_excess_and_malformed_bodies_never_succeed() {
    let server = Server::http().await;
    let transport = transport(config_for(&server));

    // T08: the connection closes before `Content-Length` bytes arrived.
    let mut truncated = Resource::new(body(8 * KIB));
    truncated.drop_at = Some(5 * KIB);
    server.put("/truncated.bin", truncated);
    let mut response = open(&transport, &request(&server.url("/truncated.bin")))
        .await
        .unwrap();
    assert!(read_all(&mut response).await.is_err());

    // More bytes than `Content-Length` announced: only the announced bytes
    // are the body.
    server.route(
        "/excess.bin",
        Route::Raw(Raw::Bytes(Bytes::from_static(
            b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nabcdEXTRA",
        ))),
    );
    let mut response = open(&transport, &request(&server.url("/excess.bin")))
        .await
        .unwrap();
    assert_eq!(read_all(&mut response).await.unwrap(), b"abcd");

    // Malformed or contradictory framing fails before any byte is read.
    let malformed: &[&'static [u8]] = &[
        b"HTTP/1.1 200 OK\r\nContent-Length: ten\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nContent-Length: 5\r\n\r\nabcd",
        b"HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\n\r\nabcd",
        b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-9/100\r\nContent-Length: 4\r\n\r\nabcd",
        b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 18446744073709551615-0/*\r\n\r\n",
    ];
    for (i, raw) in malformed.iter().enumerate() {
        let path = format!("/malformed-{i}");
        server.route(&path, Route::Raw(Raw::Bytes(Bytes::from_static(raw))));
        let mut ranged = request(&server.url(&path));
        ranged.range = Some((0, None));
        assert!(open(&transport, &ranged).await.is_err(), "{i}");
    }
}

#[tokio::test]
async fn a_body_of_unknown_length_streams_in_bounded_chunks() {
    let server = Server::http().await;
    let data = body(256 * KIB);
    let mut resource = Resource::new(data.clone());
    resource.length_header = false;
    server.put("/stream.bin", resource);
    let transport = transport(config_for(&server));
    let mut response = open(&transport, &request(&server.url("/stream.bin")))
        .await
        .unwrap();
    // T09: no length, read until the connection closes; `read_all` checks
    // that every chunk fits the engine's read buffer.
    assert_eq!(response.framing, Framing::Full { length: None });
    assert_eq!(read_all(&mut response).await.unwrap(), data);
}

#[tokio::test]
async fn redirect_budgets_are_enforced() {
    let server = Server::http().await;
    server.route("/loop", Route::Redirect(302, "/loop".into()));
    let mut config = config_for(&server);
    config.max_redirects = 3;
    let transport = transport(config);
    // T10: a loop stops after the budget, counted in requests.
    let err = open(&transport, &request(&server.url("/loop")))
        .await
        .unwrap_err();
    assert_eq!(err.code, FailureCode::UnsupportedCapability);
    assert_eq!(server.hits("/loop"), 4);
}

/// A prepared GET with credentials, redirected same-origin and then to
/// another origin of the same server.
#[tokio::test]
async fn credentials_are_stripped_on_cross_origin_redirects() {
    let server = Server::http().await;
    let data = body(KIB);
    server.route("/start", Route::Redirect(302, "/same".into()));
    server.route(
        "/same",
        Route::Redirect(307, server.url_via("localhost", "/file")),
    );
    server.put("/file", Resource::new(data.clone()));
    let transport = transport(config_for(&server));

    let mut headers = HeaderMap::new();
    headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer t"));
    headers.insert(header::COOKIE, HeaderValue::from_static("session=s"));
    let mut key = HeaderValue::from_static("k");
    key.set_sensitive(true);
    headers.insert("x-api-key", key);
    headers.insert("x-trace", HeaderValue::from_static("kept"));
    let prepared = PreparedRequest {
        method: Method::GET,
        url: Url::parse(&server.url("/start")).unwrap(),
        headers,
        body: None,
    };
    let reference = transport
        .prepare(prepared, ReplayPolicy::Idempotent)
        .unwrap();
    let transfer = lease(
        LeaseSource::PreparedRequest(reference),
        ReplayPolicy::Idempotent,
    );
    let mut response = transport
        .open_lease(&transfer, &lease_request(None))
        .await
        .unwrap();
    assert_eq!(read_all(&mut response).await.unwrap(), data);

    let log = server.log();
    let paths: Vec<&str> = log.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(paths, ["/start", "/same", "/file"]);
    for same_origin in &log[..2] {
        assert_eq!(same_origin.header("authorization"), Some("Bearer t"));
        assert_eq!(same_origin.header("cookie"), Some("session=s"));
        assert_eq!(same_origin.header("x-api-key"), Some("k"));
    }
    let other = &log[2];
    assert_eq!(other.header("authorization"), None);
    assert_eq!(other.header("cookie"), None);
    assert_eq!(other.header("x-api-key"), None);
    assert_eq!(other.header("x-trace"), Some("kept"));
    assert!(log.iter().all(|r| r.header("referer").is_none()));

    // T06 groundwork: the representation names the resource that finally
    // served it, not the one first asked for.
    let direct = async |url: String| {
        open(&transport, &request(&url))
            .await
            .unwrap()
            .representation
            .resource
    };
    let served = response.representation.resource.clone();
    assert_eq!(served, direct(server.url_via("localhost", "/file")).await);
    assert_ne!(served, direct(server.url("/file")).await);
}

#[tokio::test]
async fn bodies_are_never_redirected_to_another_origin() {
    let server = Server::http().await;
    server.route(
        "/form",
        Route::Redirect(307, server.url_via("localhost", "/elsewhere")),
    );
    let transport = transport(config_for(&server));
    let prepared = PreparedRequest {
        method: Method::POST,
        url: Url::parse(&server.url("/form")).unwrap(),
        headers: HeaderMap::new(),
        body: Some(Bytes::from_static(b"password=secret")),
    };
    // Even a request that may be repeated.
    let reference = transport
        .prepare(prepared, ReplayPolicy::Idempotent)
        .unwrap();
    let err = transport
        .submit(reference, Duration::from_secs(5), READ_BUFFER)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, BrokerError::Transport(e) if e.code == FailureCode::UnsupportedCapability),
        "{err:?}"
    );
    assert_eq!(server.hits("/elsewhere"), 0);
}

#[tokio::test]
async fn redirects_never_resend_a_non_replayable_request() {
    let server = Server::http().await;
    server.route("/charge", Route::Redirect(307, "/charge".into()));
    server.route("/moved", Route::Redirect(308, "/target".into()));
    server.route("/see-other", Route::Redirect(303, "/target".into()));
    server.put("/target", Resource::new(body(KIB)));
    let transport = transport(config_for(&server));
    let submit = async |path, replay| {
        let reference = transport.prepare(post(&server, path), replay).unwrap();
        transport
            .submit(reference, Duration::from_secs(5), READ_BUFFER)
            .await
    };

    // T12: a method-preserving redirect would send the POST again, so it
    // is not followed; the server saw it once and the outcome is unknown.
    for replay in [
        ReplayPolicy::NeverAutomaticallyReplay,
        ReplayPolicy::AdapterControlled,
    ] {
        let err = submit("/charge", replay).await.unwrap_err();
        assert!(
            matches!(&err, BrokerError::Transport(e) if e.code == FailureCode::OperationAmbiguous),
            "{replay:?}: {err:?}"
        );
    }
    assert_eq!(server.hits("/charge"), 2);

    // T11: a one-use GET redirected back to itself, directly or through
    // another URL, is not requested again either; one redirected onward to
    // a new URL, as a one-use link to its CDN, is followed.
    server.route("/once", Route::Redirect(307, "/once#again".into()));
    server.route("/a", Route::Redirect(302, "/b".into()));
    server.route("/b", Route::Redirect(301, "/a".into()));
    server.route("/link", Route::Redirect(302, "/cdn.bin".into()));
    server.put("/cdn.bin", Resource::new(body(KIB)));
    let get = async |path| {
        let request = PreparedRequest {
            method: Method::GET,
            url: Url::parse(&server.url(path)).unwrap(),
            headers: HeaderMap::new(),
            body: None,
        };
        let reference = transport
            .prepare(request, ReplayPolicy::NeverAutomaticallyReplay)
            .unwrap();
        transport
            .submit(reference, Duration::from_secs(5), READ_BUFFER)
            .await
    };
    for path in ["/once", "/a"] {
        let err = get(path).await.unwrap_err();
        assert!(
            matches!(&err, BrokerError::Transport(e) if e.code == FailureCode::OperationAmbiguous),
            "{path}: {err:?}"
        );
    }
    get("/link").await.unwrap();
    for path in ["/once", "/a", "/b", "/link", "/cdn.bin"] {
        assert_eq!(server.hits(path), 1, "{path}");
    }

    // A 303 answers the POST with another resource to GET, which repeats
    // nothing.
    submit("/see-other", ReplayPolicy::NeverAutomaticallyReplay)
        .await
        .unwrap();
    // A request that may be repeated follows a 308 with its body.
    submit("/moved", ReplayPolicy::Idempotent).await.unwrap();
    let log = server.log();
    let target: Vec<_> = log.iter().filter(|r| r.path == "/target").collect();
    assert_eq!(target.len(), 2);
    assert_eq!(
        (target[0].method.as_str(), &target[0].body[..]),
        ("GET", &b""[..])
    );
    assert_eq!(
        (target[1].method.as_str(), &target[1].body[..]),
        ("POST", &b"link=abc"[..])
    );
}

#[tokio::test]
async fn a_lease_cannot_weaken_its_prepared_request() {
    let server = Server::http().await;
    server.put("/charge", Resource::new(body(KIB)));
    let transport = transport(config_for(&server));
    let reference = transport
        .prepare(
            post(&server, "/charge"),
            ReplayPolicy::NeverAutomaticallyReplay,
        )
        .unwrap();
    // A lease that claims the consumptive request is idempotent, or leaves
    // it to the adapter, is refused before anything is sent.
    for replay in [ReplayPolicy::Idempotent, ReplayPolicy::AdapterControlled] {
        let transfer = lease(LeaseSource::PreparedRequest(reference), replay);
        for purpose in [None, Some(RequestPurpose::Retry)] {
            assert_eq!(
                transport
                    .open_lease(&transfer, &lease_request(purpose))
                    .await
                    .unwrap_err(),
                BrokerError::PolicyMismatch {
                    prepared: ReplayPolicy::NeverAutomaticallyReplay,
                    lease: replay,
                },
                "{replay:?} {purpose:?}"
            );
        }
    }
    assert_eq!(server.hits("/charge"), 0);

    // The matching lease sends it once and never again.
    let transfer = lease(
        LeaseSource::PreparedRequest(reference),
        ReplayPolicy::NeverAutomaticallyReplay,
    );
    drop(
        transport
            .open_lease(&transfer, &lease_request(None))
            .await
            .unwrap(),
    );
    assert_eq!(
        transport
            .open_lease(&transfer, &lease_request(None))
            .await
            .unwrap_err(),
        BrokerError::Refused(Replay::Reresolve)
    );
    assert_eq!(server.hits("/charge"), 1);
}

#[tokio::test]
async fn the_egress_policy_holds_on_every_hop() {
    let server = Server::http().await;
    server.put("/a.bin", Resource::new(body(KIB)));
    // 127.0.0.2 is loopback too, but not in the allowlist.
    server.route(
        "/to-other-loopback",
        Route::Redirect(302, format!("http://127.0.0.2:{}/a.bin", server.port())),
    );
    server.route(
        "/to-metadata",
        Route::Redirect(302, "http://169.254.169.254/latest/meta-data/".into()),
    );
    server.route(
        "/to-file",
        Route::Redirect(302, "file:///etc/passwd".into()),
    );
    let allowed = transport(config_for(&server));
    for path in ["/to-other-loopback", "/to-metadata", "/to-file"] {
        assert_eq!(
            code(open(&allowed, &request(&server.url(path))).await),
            FailureCode::UnsupportedCapability,
            "{path}"
        );
    }
    // Only the redirects themselves reached the server.
    assert_eq!(server.hits("/a.bin"), 0);

    // Without an allowlist, loopback is unreachable by address or by name.
    let public_only = transport(HttpConfig::default());
    assert_eq!(
        code(open(&public_only, &request(&server.url("/a.bin"))).await),
        FailureCode::UnsupportedCapability
    );
    let err = open(
        &public_only,
        &request(&server.url_via("localhost", "/a.bin")),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, FailureCode::UnsupportedCapability);
    assert!(err.message.contains("not allowed"), "{}", err.message);
    assert_eq!(server.hits("/a.bin"), 0);

    // Credentials embedded in a URL are refused before connecting.
    let embedded = format!("http://user:pw@127.0.0.1:{}/a.bin", server.port());
    assert_eq!(
        code(open(&allowed, &request(&embedded)).await),
        FailureCode::UnsupportedCapability
    );
    assert_eq!(server.hits("/a.bin"), 0);
}

#[tokio::test]
async fn https_never_downgrades_and_unknown_certificates_are_refused() {
    let https = Server::https().await;
    let http = Server::http().await;
    http.put("/a.bin", Resource::new(body(KIB)));
    https.route("/downgrade", Route::Redirect(302, http.url("/a.bin")));
    https.put("/a.bin", Resource::new(body(KIB)));
    let trusting = transport(config_for(&https));
    assert_eq!(
        code(open(&trusting, &request(&https.url("/downgrade"))).await),
        FailureCode::UnsupportedCapability
    );
    assert_eq!(http.hits("/a.bin"), 0);

    // The same server, but its CA is not trusted: the handshake fails and
    // no request is sent.
    let mut untrusting = config_for(&https);
    untrusting.extra_roots.clear();
    let err = open(&transport(untrusting), &request(&https.url("/a.bin")))
        .await
        .unwrap_err();
    assert_eq!(err.code, FailureCode::UnsupportedCapability);
    assert!(err.message.contains("TLS"), "{}", err.message);
    assert_eq!(https.hits("/a.bin"), 0);
}

#[tokio::test]
async fn statuses_are_classified() {
    let server = Server::http().await;
    for (path, status) in [
        ("/gone", "410 Gone"),
        ("/denied", "403 Forbidden"),
        ("/busy", "503 Service Unavailable"),
    ] {
        let raw = format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\n\r\n");
        server.route(path, Route::Raw(Raw::Bytes(raw.into())));
    }
    let transport = transport(config_for(&server));
    let code_of = async |path| code(open(&transport, &request(&server.url(path))).await);
    assert_eq!(code_of("/missing").await, FailureCode::LeaseNotFound);
    assert_eq!(code_of("/gone").await, FailureCode::LeaseNotFound);
    assert_eq!(code_of("/denied").await, FailureCode::LeaseExpired);
    assert_eq!(code_of("/busy").await, FailureCode::RequestTimeout);
}

fn lease(source: LeaseSource, replay: ReplayPolicy) -> Transfer {
    let offer = TransferOffer {
        candidate: Candidate {
            resource: ResourceId::from_u64(1).unwrap(),
            original_host: HostName::new("files.example").unwrap(),
            provider: ProviderId::new("fixture").unwrap(),
            account: None,
            route: RouteId::from_u64(1).unwrap(),
        },
        representation: Representation::default(),
        source,
        expires_at: None,
        replay,
        resume: ResumePolicy::Conditional,
        parallelism: Parallelism::UpTo(NonZeroU16::new(1).unwrap()),
    };
    TransferLease::new(
        offer,
        JobId::from_u64(1).unwrap(),
        Generation::INITIAL,
        AttemptId::from_u64(1).unwrap(),
    )
    .start(
        Generation::INITIAL,
        UtcTimestamp::from_system_time(SystemTime::now()),
    )
    .unwrap()
}

fn lease_request(purpose: Option<RequestPurpose>) -> LeaseRequest<'static> {
    LeaseRequest {
        purpose,
        range: None,
        validators: None,
        connect_timeout: Duration::from_secs(5),
        read_buffer: READ_BUFFER,
    }
}

fn post(server: &Server, path: &str) -> PreparedRequest {
    PreparedRequest {
        method: Method::POST,
        url: Url::parse(&server.url(path)).unwrap(),
        headers: HeaderMap::new(),
        body: Some(Bytes::from_static(b"link=abc")),
    }
}

#[tokio::test]
async fn an_opened_response_is_streamed_once_and_never_resent() {
    let server = Server::http().await;
    let data = body(64 * KIB);
    server.put("/unrestrict", Resource::new(data.clone()));
    let transport = transport(config_for(&server));
    let reference = transport
        .prepare(
            post(&server, "/unrestrict"),
            ReplayPolicy::NeverAutomaticallyReplay,
        )
        .unwrap();
    // T11: the form submission returned the download itself; the adapter
    // inspects its head and promotes the open response to a lease.
    let (opened, head) = transport
        .submit(reference, Duration::from_secs(5), READ_BUFFER)
        .await
        .unwrap();
    assert_eq!(head.representation.length, Some(data.len() as u64));
    let transfer = lease(
        LeaseSource::OpenResponse(opened),
        ReplayPolicy::NeverAutomaticallyReplay,
    );
    let mut response = transport
        .open_lease(&transfer, &lease_request(None))
        .await
        .unwrap();
    // Nothing of the body was discarded.
    assert_eq!(read_all(&mut response).await.unwrap(), data);
    // A second use or a resume of the open response is refused unsent.
    for purpose in [
        None,
        Some(RequestPurpose::Retry),
        Some(RequestPurpose::Resume),
    ] {
        assert_eq!(
            transport
                .open_lease(&transfer, &lease_request(purpose))
                .await
                .unwrap_err(),
            BrokerError::Refused(Replay::Reresolve)
        );
    }
    // The prepared request cannot be submitted again either.
    assert_eq!(
        transport
            .submit(reference, Duration::from_secs(5), READ_BUFFER)
            .await
            .unwrap_err(),
        BrokerError::Refused(Replay::Reresolve)
    );
    let posts = server.log();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].method, "POST");
    assert_eq!(&posts[0].body[..], b"link=abc");
}

#[tokio::test]
async fn one_use_requests_are_sent_once_and_idempotent_ones_may_retry() {
    let server = Server::http().await;
    server.put("/once", Resource::new(body(KIB)));
    server.put("/stable", Resource::new(body(KIB)));
    let transport = transport(config_for(&server));
    let get = |path: &str| PreparedRequest {
        method: Method::GET,
        url: Url::parse(&server.url(path)).unwrap(),
        headers: HeaderMap::new(),
        body: None,
    };

    let once = transport
        .prepare(get("/once"), ReplayPolicy::NeverAutomaticallyReplay)
        .unwrap();
    let transfer = lease(
        LeaseSource::PreparedRequest(once),
        ReplayPolicy::NeverAutomaticallyReplay,
    );
    drop(
        transport
            .open_lease(&transfer, &lease_request(None))
            .await
            .unwrap(),
    );
    // T11: a one-use link is never requested again automatically.
    for purpose in [
        None,
        Some(RequestPurpose::Retry),
        Some(RequestPurpose::Resume),
    ] {
        assert_eq!(
            transport
                .open_lease(&transfer, &lease_request(purpose))
                .await
                .unwrap_err(),
            BrokerError::Refused(Replay::Reresolve)
        );
    }
    assert_eq!(server.hits("/once"), 1);

    let stable = transport
        .prepare(get("/stable"), ReplayPolicy::Idempotent)
        .unwrap();
    let transfer = lease(
        LeaseSource::PreparedRequest(stable),
        ReplayPolicy::Idempotent,
    );
    drop(
        transport
            .open_lease(&transfer, &lease_request(None))
            .await
            .unwrap(),
    );
    drop(
        transport
            .open_lease(&transfer, &lease_request(Some(RequestPurpose::Retry)))
            .await
            .unwrap(),
    );
    assert_eq!(server.hits("/stable"), 2);

    // An adapter-controlled request waits for the adapter.
    let counted = transport
        .prepare(get("/stable"), ReplayPolicy::AdapterControlled)
        .unwrap();
    let transfer = lease(
        LeaseSource::PreparedRequest(counted),
        ReplayPolicy::AdapterControlled,
    );
    drop(
        transport
            .open_lease(&transfer, &lease_request(None))
            .await
            .unwrap(),
    );
    assert_eq!(
        transport
            .open_lease(&transfer, &lease_request(None))
            .await
            .unwrap_err(),
        BrokerError::Refused(Replay::AskAdapter)
    );
    assert_eq!(server.hits("/stable"), 3);
}

#[tokio::test]
async fn a_lost_reply_to_a_consumptive_request_is_ambiguous_and_not_resent() {
    let server = Server::http().await;
    // T12: the server reads the request and hangs up without answering, or
    // fails after doing the work.
    server.route("/charge", Route::Raw(Raw::HangUp));
    server.route(
        "/fails",
        Route::Raw(Raw::Bytes(Bytes::from_static(
            b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
        ))),
    );
    let transport = transport(config_for(&server));
    for path in ["/charge", "/fails"] {
        let reference = transport
            .prepare(post(&server, path), ReplayPolicy::NeverAutomaticallyReplay)
            .unwrap();
        let transfer = lease(
            LeaseSource::PreparedRequest(reference),
            ReplayPolicy::NeverAutomaticallyReplay,
        );
        let err = transport
            .open_lease(&transfer, &lease_request(None))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, BrokerError::Transport(e) if e.code == FailureCode::OperationAmbiguous),
            "{path}: {err:?}"
        );
        assert_eq!(
            transport
                .open_lease(&transfer, &lease_request(Some(RequestPurpose::Retry)))
                .await
                .unwrap_err(),
            BrokerError::Refused(Replay::Reresolve),
            "{path}"
        );
        assert_eq!(server.hits(path), 1, "{path}");
    }

    // The same lost reply to a plain GET is an ordinary, retryable timeout.
    server.route("/get", Route::Raw(Raw::HangUp));
    assert_eq!(
        code(open(&transport, &request(&server.url("/get"))).await),
        FailureCode::RequestTimeout
    );
}

#[tokio::test]
async fn the_broker_is_bounded() {
    let server = Server::http().await;
    server.put("/a", Resource::new(body(KIB)));
    server.put("/b", Resource::new(body(KIB)));
    let mut config = config_for(&server);
    config.max_prepared = NonZeroUsize::new(2).unwrap();
    config.max_open_responses = NonZeroUsize::new(1).unwrap();
    let transport = transport(config);
    let first = transport
        .prepare(post(&server, "/a"), ReplayPolicy::Idempotent)
        .unwrap();
    let second = transport
        .prepare(post(&server, "/b"), ReplayPolicy::Idempotent)
        .unwrap();
    // One open response is held; another is refused before it is sent.
    let submit = async |request| {
        transport
            .submit(request, Duration::from_secs(5), READ_BUFFER)
            .await
    };
    let (held, _) = submit(first).await.unwrap();
    assert_eq!(submit(second).await.unwrap_err(), BrokerError::Full);
    assert_eq!(server.hits("/b"), 0);
    transport.discard(held);
    submit(second).await.unwrap();
    assert_eq!(
        transport
            .prepare(post(&server, "/c"), ReplayPolicy::Idempotent)
            .unwrap_err(),
        BrokerError::Full
    );
    transport.forget(first);
    assert!(
        transport
            .prepare(post(&server, "/c"), ReplayPolicy::Idempotent)
            .is_ok()
    );
}
