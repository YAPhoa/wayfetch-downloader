//! A deterministic local HTTP/HTTPS server for transfer tests. It speaks
//! just enough HTTP/1.1 to misbehave on purpose, logs every request, and
//! generates its TLS certificates per server.

#![allow(dead_code, reason = "each test file uses part of the fixture")]

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair, SanType};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::{self, pki_types};
use wayfetch_net::CertificateDer;

/// A file the server holds.
#[derive(Debug, Clone)]
pub struct Resource {
    pub body: Bytes,
    pub etag: Option<String>,
    /// Whether `Range` is honored at all.
    pub ranges: bool,
    /// Whether `If-Range` is honored; a server that ignores it sends a
    /// changed representation's bytes for the old range.
    pub if_range: bool,
    /// Whether `Content-Length` is sent; without it the body ends when the
    /// connection closes.
    pub length_header: bool,
    /// The body stops sending at this absolute offset until cleared.
    pub stall_at: Option<usize>,
    /// The connection closes at this absolute offset.
    pub drop_at: Option<usize>,
    /// Every range request is answered `416` with this complete length.
    pub unsatisfiable: Option<u64>,
    /// Extra response headers.
    pub headers: Vec<(String, String)>,
}

impl Resource {
    pub fn new(body: Bytes) -> Self {
        Self {
            body,
            etag: Some("\"v1\"".into()),
            ranges: true,
            if_range: true,
            length_header: true,
            stall_at: None,
            drop_at: None,
            unsatisfiable: None,
            headers: Vec::new(),
        }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

/// A canned answer.
#[derive(Debug, Clone)]
pub enum Raw {
    /// Writes these bytes verbatim, then closes.
    Bytes(Bytes),
    /// Never answers.
    Hang,
    /// Closes as soon as the request was read.
    HangUp,
}

/// What a path serves.
#[derive(Debug, Clone)]
pub enum Route {
    File(Resource),
    /// A redirect with this status to this location.
    Redirect(u16, String),
    Raw(Raw),
}

/// A request as the server read it.
#[derive(Debug, Clone)]
pub struct Logged {
    pub method: String,
    pub path: String,
    /// Header names in lower case.
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

impl Logged {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// The requested range as the engine expresses it.
    pub fn range(&self) -> Option<(u64, Option<u64>)> {
        let spec = self.header("range")?.strip_prefix("bytes=")?;
        let (start, end) = spec.split_once('-')?;
        let end = (!end.is_empty()).then(|| end.parse::<u64>().unwrap() + 1);
        Some((start.parse().unwrap(), end))
    }
}

#[derive(Debug, Default)]
struct State {
    routes: Mutex<HashMap<String, Route>>,
    log: Mutex<Vec<Logged>>,
}

/// A running server on `127.0.0.1`.
pub struct Server {
    addr: SocketAddr,
    tls: Option<CertificateDer<'static>>,
    state: Arc<State>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Longest request head the server reads.
const MAX_HEAD: usize = 64 * 1024;

impl Server {
    /// A plain HTTP server.
    pub async fn http() -> Self {
        Self::start(Ipv4Addr::LOCALHOST.into(), None).await
    }

    /// An HTTPS server with a certificate for `127.0.0.1` and `localhost`
    /// from a fresh CA; [`Server::ca`] is the root to trust.
    pub async fn https() -> Self {
        Self::https_on(Ipv4Addr::LOCALHOST.into()).await
    }

    /// An HTTPS server listening on `ip` instead, with a certificate for
    /// `ip` and `localhost`: for a client in a container, which reaches the
    /// host on its bridge address.
    pub async fn https_on(ip: IpAddr) -> Self {
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca = CertifiedIssuer::self_signed(ca_params, ca_key).unwrap();
        let leaf_key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(vec!["localhost".to_owned()]).unwrap();
        params.subject_alt_names.push(SanType::IpAddress(ip));
        let leaf = params.signed_by(&leaf_key, &ca).unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![leaf.der().clone()],
            pki_types::PrivateKeyDer::Pkcs8(leaf_key.serialize_der().into()),
        )
        .unwrap();
        let ca_der = ca.as_ref().der().clone();
        Self::start(ip, Some((TlsAcceptor::from(Arc::new(config)), ca_der))).await
    }

    async fn start(ip: IpAddr, tls: Option<(TlsAcceptor, CertificateDer<'static>)>) -> Self {
        let listener = TcpListener::bind((ip, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(State::default());
        let (acceptor, ca) = match tls {
            Some((acceptor, ca)) => (Some(acceptor), Some(ca)),
            None => (None, None),
        };
        let task = {
            let state = state.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((socket, _)) = listener.accept().await else {
                        return;
                    };
                    let state = state.clone();
                    let acceptor = acceptor.clone();
                    tokio::spawn(async move {
                        match acceptor {
                            None => serve(socket, state).await,
                            Some(acceptor) => {
                                // A client that refuses the certificate ends
                                // the handshake; nothing was requested.
                                if let Ok(stream) = acceptor.accept(socket).await {
                                    serve(stream, state).await;
                                }
                            }
                        }
                    });
                }
            })
        };
        Self {
            addr,
            tls: ca,
            state,
            task,
        }
    }

    /// The CA to trust for an HTTPS server.
    pub fn ca(&self) -> CertificateDer<'static> {
        self.tls.clone().expect("an HTTPS server")
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// The URL of `path` through the address the server listens on.
    pub fn url(&self, path: &str) -> String {
        self.url_via(&self.addr.ip().to_string(), path)
    }

    /// The URL of `path` through `host`, a different origin on the same
    /// server.
    pub fn url_via(&self, host: &str, path: &str) -> String {
        let scheme = if self.tls.is_some() { "https" } else { "http" };
        format!("{scheme}://{host}:{}{path}", self.addr.port())
    }

    pub fn route(&self, path: &str, route: Route) {
        self.state
            .routes
            .lock()
            .unwrap()
            .insert(path.to_owned(), route);
    }

    pub fn put(&self, path: &str, resource: Resource) {
        self.route(path, Route::File(resource));
    }

    pub fn update(&self, path: &str, change: impl FnOnce(&mut Resource)) {
        match self.state.routes.lock().unwrap().get_mut(path) {
            Some(Route::File(resource)) => change(resource),
            other => panic!("{path} is {other:?}"),
        }
    }

    /// Every request so far.
    pub fn log(&self) -> Vec<Logged> {
        self.state.log.lock().unwrap().clone()
    }

    /// Requests for `path` so far.
    pub fn hits(&self, path: &str) -> usize {
        self.log().iter().filter(|r| r.path == path).count()
    }

    /// Ranges requested for `path`, in order.
    pub fn ranges(&self, path: &str) -> Vec<Option<(u64, Option<u64>)>> {
        self.log()
            .iter()
            .filter(|r| r.path == path)
            .map(Logged::range)
            .collect()
    }
}

/// Serves one request on `stream`, then closes it.
async fn serve<S: AsyncRead + AsyncWrite + Unpin>(mut stream: S, state: Arc<State>) {
    let Some(request) = read_request(&mut stream).await else {
        return;
    };
    state.log.lock().unwrap().push(request.clone());
    let route = state.routes.lock().unwrap().get(&request.path).cloned();
    match route {
        None => {
            let _ = stream
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
        }
        Some(Route::Redirect(status, location)) => {
            let head = format!(
                "HTTP/1.1 {status} Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = stream.write_all(head.as_bytes()).await;
        }
        Some(Route::Raw(Raw::Bytes(bytes))) => {
            let _ = stream.write_all(&bytes).await;
        }
        Some(Route::Raw(Raw::Hang)) => std::future::pending().await,
        Some(Route::Raw(Raw::HangUp)) => {}
        Some(Route::File(resource)) => serve_file(&mut stream, &state, &request, resource).await,
    }
    let _ = stream.shutdown().await;
}

async fn serve_file<S: AsyncWrite + Unpin>(
    stream: &mut S,
    state: &State,
    request: &Logged,
    resource: Resource,
) {
    let len = resource.body.len() as u64;
    let validator_matches = match request.header("if-range") {
        None => true,
        Some(v) => !resource.if_range || resource.etag.as_deref() == Some(v),
    };
    let range = request
        .range()
        .filter(|_| resource.ranges && validator_matches);
    let mut head = String::new();
    let (start, end) = match (range, resource.unsatisfiable) {
        (Some(_), Some(complete)) => {
            let head = format!(
                "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{complete}\r\n{}Content-Length: 0\r\nConnection: close\r\n\r\n",
                etag_header(&resource)
            );
            let _ = stream.write_all(head.as_bytes()).await;
            return;
        }
        (Some((start, end)), None) => {
            let end = end.unwrap_or(len).min(len);
            head.push_str(&format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{}/{len}\r\n",
                end - 1
            ));
            (start, end)
        }
        (None, _) => {
            head.push_str("HTTP/1.1 200 OK\r\n");
            (0, len)
        }
    };
    head.push_str(&etag_header(&resource));
    if resource.length_header {
        head.push_str(&format!("Content-Length: {}\r\n", end - start));
    }
    for (name, value) in &resource.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("Connection: close\r\n\r\n");
    if stream.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    let path = request.path.clone();
    let mut offset = usize::try_from(start).unwrap();
    let end = usize::try_from(end).unwrap();
    loop {
        let (stall, drop_at) = match state.routes.lock().unwrap().get(&path) {
            Some(Route::File(r)) => (r.stall_at, r.drop_at),
            _ => (None, None),
        };
        if drop_at.is_some_and(|at| offset >= at) {
            return;
        }
        // A stall at the end holds the connection open after the last byte.
        if stall.is_some_and(|at| offset >= at) {
            tokio::time::sleep(Duration::from_millis(5)).await;
            continue;
        }
        if offset == end {
            return;
        }
        let mut next = (offset + 16 * 1024).min(end);
        for limit in [stall, drop_at].into_iter().flatten() {
            if limit > offset {
                next = next.min(limit);
            }
        }
        if stream
            .write_all(&resource.body[offset..next])
            .await
            .is_err()
        {
            return;
        }
        let _ = stream.flush().await;
        offset = next;
    }
}

fn etag_header(resource: &Resource) -> String {
    resource
        .etag
        .as_ref()
        .map(|e| format!("ETag: {e}\r\n"))
        .unwrap_or_default()
}

async fn read_request<S: AsyncRead + Unpin>(stream: &mut S) -> Option<Logged> {
    let mut buf = Vec::with_capacity(4096);
    let head_len = loop {
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > MAX_HEAD {
            return None;
        }
    };
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Request::new(&mut headers);
    parsed.parse(&buf[..head_len]).ok()?;
    let headers: Vec<(String, String)> = parsed
        .headers
        .iter()
        .map(|h| {
            (
                h.name.to_ascii_lowercase(),
                String::from_utf8_lossy(h.value).into_owned(),
            )
        })
        .collect();
    let length: usize = headers
        .iter()
        .find(|(n, _)| n == "content-length")
        .map_or(0, |(_, v)| v.parse().unwrap());
    let mut body = buf[head_len..].to_vec();
    while body.len() < length {
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Some(Logged {
        method: parsed.method?.to_owned(),
        path: parsed.path?.to_owned(),
        headers,
        body: body.into(),
    })
}

/// A transport configuration that trusts `server` and may reach it.
pub fn config_for(server: &Server) -> wayfetch_net::HttpConfig {
    config_trusting(server.tls.clone())
}

/// A transport configuration that may reach a local server and trusts `ca`
/// in addition to the platform roots.
pub fn config_trusting(ca: Option<CertificateDer<'static>>) -> wayfetch_net::HttpConfig {
    let mut config = wayfetch_net::HttpConfig::default();
    config
        .egress
        .allowed_networks
        .push("127.0.0.1/32".parse().unwrap());
    config.extra_roots.extend(ca);
    config
}

/// An HTTPS server and engines that download from it with the real
/// transport, configured by `configure`.
pub async fn https_env(
    configure: impl FnOnce(&mut wayfetch_net::HttpConfig),
) -> crate::engine_support::Env<Server> {
    let server = Server::https().await;
    let mut config = config_for(&server);
    configure(&mut config);
    let transport = wayfetch_net::HttpTransport::new(config).unwrap();
    crate::engine_support::Env::new(server, Arc::new(transport))
}
