//! Dropbox file links against a local stand-in for Dropbox's site and its
//! download servers, through the real HTTP transport. Nothing here reaches
//! Dropbox.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use url::Url;
use wayfetch_adapters::Endpoint;
use wayfetch_adapters::dropbox::Dropbox;
use wayfetch_engine::domain::failure::FailureCode;
use wayfetch_engine::hosting::{FileHost, HostCheck};
use wayfetch_engine::protocol::query::CheckReason;
use wayfetch_engine::transfer::transport::{OpenRequest, Reach, Transport};
use wayfetch_net::{HttpConfig, HttpTransport};

const SIZE: usize = 300_000;
const NAME: &str = "Quarterly report €.pdf";
const FILE: &str = "/scl/fi/abc123/report.pdf";
const ETAG: &str = "\"rev-1\"";

fn content() -> Vec<u8> {
    (0..SIZE).map(|i| ((i * 13 + 5) % 251) as u8).collect()
}

fn link(path: &str, rlkey: &str) -> Url {
    Url::parse(&format!("https://www.dropbox.com{path}?rlkey={rlkey}&dl=0")).unwrap()
}

#[derive(Default)]
struct State {
    /// Request targets, in order.
    log: Vec<String>,
    /// Temporary URLs handed out so far.
    issued: AtomicUsize,
    /// The `ETag` the download servers send, if any.
    etag: Option<&'static str>,
}

struct Server {
    addr: SocketAddr,
    state: Arc<Mutex<State>>,
}

impl Server {
    async fn start() -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(State {
            etag: Some(ETAG),
            ..State::default()
        }));
        let shared = state.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                tokio::spawn(serve(stream, shared.clone()));
            }
        });
        Self { addr, state }
    }

    fn dropbox(&self) -> Dropbox {
        let site = Endpoint::custom(&format!("http://{}/", self.addr)).unwrap();
        Dropbox::new(transport(), site)
    }

    fn without_etag(&self) {
        self.state.lock().unwrap().etag = None;
    }

    fn log(&self) -> Vec<String> {
        self.state.lock().unwrap().log.clone()
    }
}

/// The site redirects a file link with `dl=1` to a fresh temporary URL;
/// other paths stand for links that are gone, suspended, or behind a
/// password page.
async fn serve(mut stream: TcpStream, state: Arc<Mutex<State>>) {
    let mut buf = Vec::new();
    loop {
        let mut chunk = [0; 4096];
        let n = stream.read(&mut chunk).await.unwrap();
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buf).to_string();
    let target = head.split(' ').nth(1).unwrap().to_owned();
    let range = head.lines().find_map(|l| {
        let (name, value) = l.split_once(':')?;
        name.eq_ignore_ascii_case("range")
            .then(|| value.trim().strip_prefix("bytes=").map(str::to_owned))?
    });
    let (issued, etag) = {
        let mut state = state.lock().unwrap();
        state.log.push(target.clone());
        (state.issued.load(Ordering::Relaxed), state.etag)
    };
    let etag = etag.map_or(String::new(), |e| format!("ETag: {e}\r\n"));
    let url = Url::parse(&format!("http://stand.in{target}")).unwrap();
    let dl = url.query_pairs().any(|(k, v)| k == "dl" && v == "1");
    let rlkey = url
        .query_pairs()
        .find(|(k, _)| k == "rlkey")
        .map(|(_, v)| v.into_owned());
    let plain = |status: &str, kind: &str, body: &str| {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    };
    let response = match (url.path(), rlkey.as_deref()) {
        (FILE, Some("k1")) if dl => {
            state.lock().unwrap().issued.fetch_add(1, Ordering::Relaxed);
            format!(
                "HTTP/1.1 302 Found\r\nLocation: /cd/0/get/token{issued}/file?dl=1\r\n\
                 Content-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .into_bytes()
        }
        (FILE, Some("suspended")) => plain("429 Too Many Requests", "text/html", "<p>banned</p>"),
        (FILE, Some("password")) => plain("200 OK", "text/html", "<form>password</form>"),
        ("/scl/fi/broken/x.bin", _) => plain("503 Service Unavailable", "text/plain", "busy"),
        (path, _) if path.starts_with("/cd/0/get/token") => {
            let (start, end) = match range.as_deref().and_then(|r| r.split_once('-')) {
                Some((a, b)) => (
                    a.parse::<usize>().unwrap(),
                    b.parse::<usize>().map_or(SIZE - 1, |b| b.min(SIZE - 1)),
                ),
                None => (0, SIZE - 1),
            };
            let partial = range.is_some();
            let mut out = format!(
                "HTTP/1.1 {}\r\nContent-Type: application/pdf\r\n\
                 Content-Disposition: attachment; filename=\"report.pdf\"; \
                 filename*=UTF-8''Quarterly%20report%20%E2%82%AC.pdf\r\n\
                 Content-Length: {}\r\n{}{etag}Accept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                if partial {
                    "206 Partial Content"
                } else {
                    "200 OK"
                },
                end + 1 - start,
                if partial {
                    format!("Content-Range: bytes {start}-{end}/{SIZE}\r\n")
                } else {
                    String::new()
                }
            )
            .into_bytes();
            out.extend_from_slice(&content()[start..=end]);
            out
        }
        _ => plain("404 Not Found", "text/html", "<p>gone</p>"),
    };
    let _ = stream.write_all(&response).await;
    let _ = stream.shutdown().await;
}

fn transport() -> Arc<HttpTransport> {
    let mut config = HttpConfig::default();
    config
        .egress
        .allowed_networks
        .push("127.0.0.1/32".parse().unwrap());
    Arc::new(HttpTransport::new(config).unwrap())
}

async fn download(url: &Url, from: u64) -> Vec<u8> {
    let request = OpenRequest {
        url: url.clone(),
        range: (from > 0).then_some((from, None)),
        validators: None,
        connect_timeout: Duration::from_secs(5),
        read_buffer: 64 * 1024,
        user_agent: None,
        reach: Reach::Allowed,
    };
    let mut response = transport().open(&request).await.unwrap();
    let mut out = Vec::new();
    while let Some(chunk) = response.body.next_chunk().await.unwrap() {
        out.extend_from_slice(&chunk);
    }
    out
}

#[tokio::test]
async fn a_file_link_is_checked_by_its_first_byte() {
    let server = Server::start().await;
    let dropbox = server.dropbox();
    let link = link(FILE, "k1");
    assert!(dropbox.recognizes(&link));
    assert_eq!(
        dropbox.check(&link).await,
        HostCheck::File {
            name: Some(NAME.into()),
            size: Some(SIZE as u64),
        }
    );
    assert_eq!(
        server.log(),
        [
            format!("{FILE}?rlkey=k1&dl=1"),
            "/cd/0/get/token0/file?dl=1".to_owned()
        ]
    );
}

#[tokio::test]
async fn each_attempt_gets_a_fresh_url_under_one_identity_and_resumes() {
    let server = Server::start().await;
    let dropbox = server.dropbox();
    let link = link(FILE, "k1");
    let first = dropbox.resolve(&link).await.unwrap();
    let second = dropbox.resolve(&link).await.unwrap();
    assert_ne!(first.url, second.url);
    assert_eq!(first.identity, second.identity);
    assert!(!first.identity.contains("k1"));
    assert_eq!(first.size, SIZE as u64);
    assert_eq!(first.name.as_deref(), Some(NAME));
    assert!(first.encoding.is_none());
    let version = first.validators.as_ref().unwrap();
    assert_eq!(version.strong_etag.as_deref(), Some(ETAG));
    assert!(version.is_trustworthy());
    assert_eq!(download(&first.url, 0).await, content());
    assert_eq!(download(&second.url, 1000).await, content()[1000..]);
}

#[tokio::test]
async fn a_file_without_validators_cannot_be_continued() {
    let server = Server::start().await;
    server.without_etag();
    let hosted = server.dropbox().resolve(&link(FILE, "k1")).await.unwrap();
    let version = hosted.validators.unwrap();
    assert_eq!(version.strong_etag, None);
    assert!(!version.is_trustworthy(), "the engine restarts instead");
}

#[tokio::test]
async fn dropboxs_refusals_have_their_meaning() {
    let server = Server::start().await;
    let dropbox = server.dropbox();

    let gone = link(FILE, "wrong-key");
    assert!(matches!(dropbox.check(&gone).await, HostCheck::Offline(_)));
    let e = dropbox.resolve(&gone).await.unwrap_err();
    assert_eq!(e.code, FailureCode::ResourceDeleted);

    let suspended = link(FILE, "suspended");
    assert!(matches!(
        dropbox.check(&suspended).await,
        HostCheck::Unknown(CheckReason::RateLimited, _)
    ));
    let e = dropbox.resolve(&suspended).await.unwrap_err();
    assert_eq!(e.code, FailureCode::HostUnavailableViaProvider);
    assert!(
        e.retry_after
            .is_some_and(|d| d >= Duration::from_secs(86_400))
    );

    let password = link(FILE, "password");
    assert!(matches!(
        dropbox.check(&password).await,
        HostCheck::Unknown(CheckReason::AccessDenied, why) if why.contains("password")
    ));
    let e = dropbox.resolve(&password).await.unwrap_err();
    assert_eq!(e.code, FailureCode::UnsupportedCapability);

    let broken = link("/scl/fi/broken/x.bin", "k1");
    assert!(matches!(
        dropbox.check(&broken).await,
        HostCheck::Unknown(CheckReason::ServerError, _)
    ));
    assert!(
        dropbox
            .resolve(&broken)
            .await
            .unwrap_err()
            .retry_after
            .is_some()
    );

    assert_eq!(
        dropbox.refused(429).map(|e| e.code),
        Some(FailureCode::HostUnavailableViaProvider)
    );
    assert_eq!(dropbox.refused(500), None);
}

#[tokio::test]
async fn a_folder_link_is_recognized_but_not_fetched() {
    let server = Server::start().await;
    let dropbox = server.dropbox();
    let folder = link("/scl/fo/abc123/def456", "k1");
    assert!(dropbox.recognizes(&folder));
    assert!(matches!(
        dropbox.check(&folder).await,
        HostCheck::Unknown(CheckReason::Unreadable, why) if why.contains("folder")
    ));
    let e = dropbox.resolve(&folder).await.unwrap_err();
    assert_eq!(e.code, FailureCode::UnsupportedCapability);
    assert!(server.log().is_empty());
}

#[tokio::test]
async fn an_unreachable_site_leaves_the_check_unknown() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let site = Endpoint::custom(&format!("http://{addr}/")).unwrap();
    let dropbox = Dropbox::new(transport(), site);
    assert!(matches!(
        dropbox.check(&link(FILE, "k1")).await,
        HostCheck::Unknown(CheckReason::Unreachable, _)
    ));
    let e = dropbox.resolve(&link(FILE, "k1")).await.unwrap_err();
    assert_eq!(e.code, FailureCode::HostUnavailableViaProvider);
}
