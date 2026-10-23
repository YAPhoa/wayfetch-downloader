//! Pixeldrain links against a local stand-in for Pixeldrain's API, through
//! the real HTTP transport. Nothing here reaches Pixeldrain.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use url::Url;
use wayfetch_adapters::Endpoint;
use wayfetch_adapters::pixeldrain::Pixeldrain;
use wayfetch_engine::domain::failure::FailureCode;
use wayfetch_engine::hosting::{FileHost, HostCheck, HostedFile};
use wayfetch_engine::protocol::query::CheckReason;
use wayfetch_engine::transfer::transport::{OpenRequest, Reach, Transport};
use wayfetch_net::{HttpConfig, HttpTransport};

const FILE: &str = "Fi1e_abc";
const LIST: &str = "L1st-xyz";
const SIZE: usize = 300_007;

fn content() -> Vec<u8> {
    (0..SIZE).map(|i| ((i * 13 + 5) % 251) as u8).collect()
}

fn file_link() -> Url {
    Url::parse(&format!("https://pixeldrain.com/u/{FILE}")).unwrap()
}

#[derive(Default)]
struct State {
    /// Paths requested.
    log: Vec<String>,
    /// Error answers by path, instead of the normal answer.
    errors: HashMap<String, (u16, &'static str)>,
    /// What the file's details say about its availability.
    availability: &'static str,
    /// The SHA-256 the details give.
    hash: Option<String>,
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
            hash: Some(hex::encode(Sha256::digest(content()))),
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

    fn pixeldrain(&self) -> Pixeldrain {
        Pixeldrain::new(
            transport(),
            Endpoint::custom(&format!("http://{}/api/", self.addr)).unwrap(),
        )
    }

    fn error(&self, path: &str, status: u16, value: &'static str) {
        let mut state = self.state.lock().unwrap();
        state.errors.insert(path.into(), (status, value));
    }

    fn log(&self) -> Vec<String> {
        self.state.lock().unwrap().log.clone()
    }
}

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
    let mut lines = head.lines();
    let path = lines.next().unwrap().split(' ').nth(1).unwrap().to_owned();
    let range: Option<usize> = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.eq_ignore_ascii_case("range"))
        .and_then(|(_, v)| {
            v.trim()
                .strip_prefix("bytes=")?
                .strip_suffix('-')?
                .parse()
                .ok()
        });
    let (errors, availability, hash) = {
        let mut state = state.lock().unwrap();
        state.log.push(path.clone());
        (state.errors.clone(), state.availability, state.hash.clone())
    };
    let json = |status: u16, body: String| {
        format!(
            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    };
    let response = if let Some((status, value)) = errors.get(&path) {
        json(
            *status,
            json!({ "success": false, "value": value, "message": "no" }).to_string(),
        )
    } else if path == format!("/api/file/{FILE}/info") {
        json(
            200,
            json!({
                "id": FILE, "name": "sample.bin", "size": SIZE, "mime_type": "application/octet-stream",
                "hash_sha256": hash, "availability": availability, "availability_message": "",
                "can_download": true,
            })
            .to_string(),
        )
    } else if path == format!("/api/list/{LIST}") {
        json(
            200,
            json!({
                "success": true, "id": LIST, "title": "Album", "file_count": 3,
                "files": [
                    { "id": FILE, "name": "sample.bin", "size": SIZE, "detail_href": format!("/file/{FILE}/info") },
                    { "id": "Sec0nd_1", "name": "b.txt", "size": 1 },
                    { "id": "bad,id", "name": "c.txt", "size": 1 },
                ],
            })
            .to_string(),
        )
    } else if path == format!("/api/file/{FILE}") {
        let start = range.unwrap_or(0);
        let data = content();
        let mut out = format!(
            "HTTP/1.1 {}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\n\
             {}Connection: close\r\n\r\n",
            if start > 0 {
                "206 Partial Content"
            } else {
                "200 OK"
            },
            SIZE - start,
            if start > 0 {
                format!("Content-Range: bytes {start}-{}/{SIZE}\r\n", SIZE - 1)
            } else {
                String::new()
            }
        )
        .into_bytes();
        out.extend_from_slice(&data[start..]);
        out
    } else {
        json(
            404,
            json!({ "success": false, "value": "not_found" }).to_string(),
        )
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

/// Downloads `hosted` from `from` through the transport.
async fn download(hosted: &HostedFile, from: u64) -> Vec<u8> {
    let request = OpenRequest {
        url: hosted.url.clone(),
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
async fn a_file_is_checked_by_its_details_alone() {
    let server = Server::start().await;
    let host = server.pixeldrain();
    assert!(host.recognizes(&file_link()));
    assert_eq!(
        host.check(&file_link()).await,
        HostCheck::File {
            name: Some("sample.bin".into()),
            size: Some(SIZE as u64),
        }
    );
    assert_eq!(server.log(), [format!("/api/file/{FILE}/info")]);
}

#[tokio::test]
async fn a_file_downloads_from_any_offset_and_is_checked_against_its_sha256() {
    let server = Server::start().await;
    let host = server.pixeldrain();
    let short = Url::parse(&format!("https://pixeldra.in/u/{FILE}")).unwrap();
    let hosted = host.resolve(&short).await.unwrap();
    assert_eq!(hosted.size, SIZE as u64);
    assert_eq!(hosted.name.as_deref(), Some("sample.bin"));
    assert_eq!(hosted.url.path(), format!("/api/file/{FILE}"));
    let hash = hex::encode(Sha256::digest(content()));
    assert_eq!(hosted.identity, format!("{FILE};{SIZE};{hash}"));
    assert!(hosted.validators.is_none());
    let whole = download(&hosted, 0).await;
    assert!(whole == content());
    let encoding = hosted.encoding.as_ref().expect("a SHA-256 check");
    let mut check = encoding.verifier();
    check.update(&whole);
    assert!(check.verify());
    let mut corrupt = whole.clone();
    corrupt[SIZE / 2] ^= 1;
    let mut check = encoding.verifier();
    check.update(&corrupt);
    assert!(!check.verify());
    let tail = download(&hosted, 123_457).await;
    assert!(tail == content()[123_457..]);
    // Without a hash the file still downloads, under its ID and size.
    server.state.lock().unwrap().hash = None;
    let unhashed = host.resolve(&file_link()).await.unwrap();
    assert_eq!(unhashed.identity, format!("{FILE};{SIZE}"));
    assert!(unhashed.encoding.is_none());
}

#[tokio::test]
async fn a_list_is_listed_as_its_files() {
    let server = Server::start().await;
    let host = server.pixeldrain();
    let list = Url::parse(&format!("https://pixeldrain.com/l/{LIST}#item=1")).unwrap();
    let HostCheck::Folder(files) = host.check(&list).await else {
        panic!("not listed as a folder");
    };
    let expected: Vec<Url> = [FILE, "Sec0nd_1"]
        .iter()
        .map(|id| Url::parse(&format!("https://pixeldrain.com/u/{id}")).unwrap())
        .collect();
    assert_eq!(files, expected);
    assert!(files.iter().all(|f| host.recognizes(f)));
    assert_eq!(
        host.resolve(&list).await.unwrap_err().code,
        FailureCode::UnsupportedCapability
    );
}

#[tokio::test]
async fn filesystem_shares_are_reported_as_not_supported_yet() {
    let server = Server::start().await;
    let host = server.pixeldrain();
    let share = Url::parse("https://pixeldrain.com/d/abcd1234").unwrap();
    assert!(host.recognizes(&share));
    assert!(matches!(
        host.check(&share).await,
        HostCheck::Unknown(CheckReason::Unreadable, _)
    ));
    assert_eq!(
        host.resolve(&share).await.unwrap_err().code,
        FailureCode::UnsupportedCapability
    );
    assert!(server.log().is_empty(), "nothing is asked");
}

#[tokio::test]
async fn pixeldrains_refusals_have_their_meaning() {
    let server = Server::start().await;
    let host = server.pixeldrain();
    let gone = Url::parse("https://pixeldrain.com/u/Gone_123").unwrap();
    assert!(matches!(host.check(&gone).await, HostCheck::Offline(_)));
    assert_eq!(
        host.resolve(&gone).await.unwrap_err().code,
        FailureCode::ResourceDeleted
    );

    // A captcha shown in the details stops the download before it starts.
    server.state.lock().unwrap().availability = "file_rate_limited_captcha_required";
    let captcha = host.resolve(&file_link()).await.unwrap_err();
    assert_eq!(captcha.code, FailureCode::PermissionDenied);
    assert!(captcha.message.contains("captcha"));
    assert!(matches!(
        host.check(&file_link()).await,
        HostCheck::Unknown(CheckReason::AccessDenied, _)
    ));
    // Limits and takedowns shown there keep their meaning too.
    server.state.lock().unwrap().availability = "transfer_limit_exceeded";
    let limited = host.resolve(&file_link()).await.unwrap_err();
    assert_eq!(limited.code, FailureCode::HostUnavailableViaProvider);
    assert!(limited.retry_after.is_some());
    server.state.lock().unwrap().availability = "unavailable_for_legal_reasons";
    assert!(matches!(
        host.check(&file_link()).await,
        HostCheck::Offline(_)
    ));
    assert_eq!(
        host.resolve(&file_link()).await.unwrap_err().code,
        FailureCode::ResourceDeleted
    );
    server.state.lock().unwrap().availability = "";

    // A SHA-256 that is not one never turns the check off.
    server.state.lock().unwrap().hash = Some("abc123".into());
    assert_eq!(
        host.resolve(&file_link()).await.unwrap_err().code,
        FailureCode::AdapterParseFailure
    );
    server.state.lock().unwrap().hash = None;

    let info = format!("/api/file/{FILE}/info");
    server.error(&info, 451, "unavailable_for_legal_reasons");
    assert!(matches!(
        host.check(&file_link()).await,
        HostCheck::Offline(_)
    ));
    server.error(&info, 403, "transfer_limit_exceeded");
    let limited = host.resolve(&file_link()).await.unwrap_err();
    assert_eq!(limited.code, FailureCode::HostUnavailableViaProvider);
    assert!(limited.retry_after.is_some());
    server.error(&info, 403, "max_concurrent_downloads");
    assert!(matches!(
        host.check(&file_link()).await,
        HostCheck::Unknown(CheckReason::RateLimited, _)
    ));
    server.error(&format!("/api/list/{LIST}"), 404, "not_found");
    let list = Url::parse(&format!("https://pixeldrain.com/l/{LIST}")).unwrap();
    assert!(matches!(host.check(&list).await, HostCheck::Offline(_)));

    // Refusals of the bytes themselves.
    assert_eq!(
        host.refused(403).map(|e| e.code),
        Some(FailureCode::PermissionDenied)
    );
    assert_eq!(
        host.refused(451).map(|e| e.code),
        Some(FailureCode::ResourceDeleted)
    );
    assert!(host.refused(429).is_some_and(|e| e.retry_after.is_some()));
    assert_eq!(host.refused(416), None);
}

#[tokio::test]
async fn an_unreachable_api_leaves_the_check_unknown() {
    let host = Pixeldrain::new(
        transport(),
        Endpoint::custom("http://127.0.0.1:9/api/").unwrap(),
    );
    assert!(matches!(
        host.check(&file_link()).await,
        HostCheck::Unknown(CheckReason::Unreachable, _)
    ));
    assert_eq!(
        host.resolve(&file_link()).await.unwrap_err().code,
        FailureCode::HostUnavailableViaProvider
    );
}
