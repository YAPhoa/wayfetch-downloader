//! MEGA links against a local stand-in for MEGA's API and storage, through
//! the real HTTP transport. Nothing here reaches MEGA. The file and its key
//! are the test vector the crypto module checks against a reference.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use url::Url;
use wayfetch_adapters::Endpoint;
use wayfetch_adapters::mega::Mega;
use wayfetch_adapters::mega::crypto::FileKey;
use wayfetch_adapters::mega::link::MegaLink;
use wayfetch_engine::domain::failure::FailureCode;
use wayfetch_engine::hosting::{FileHost, HostCheck, HostedFile};
use wayfetch_engine::protocol::query::CheckReason;
use wayfetch_engine::transfer::transport::{OpenRequest, Reach, Transport};
use wayfetch_net::{HttpConfig, HttpTransport};

const KEY: &str = "ERMRFxETER8-P_sk6eh2pQECAwQFBgcIJibhP_X1aLo";
const ATTRIBUTES: &str = "15Ukrmp1IMoZnwKA3m5JcmAfMFJw_XJRv-F2R1LTvfA";
const SIZE: usize = 2_621_450;
const FOLDER_KEY: &str = "ZGVmZ2hpamtsbW5vcHFycw";
const NODE_KEY: &str = "-50QbYsFMxwD_E-zSkk8xoF-K9Dq8NOcYHd6KUAgayg";
const FILE: &str = "AbCd-_12";
const FOLDER: &str = "FoLd3r_x";

fn plain() -> Vec<u8> {
    (0..SIZE).map(|i| ((i * 31 + 7) % 251) as u8).collect()
}

fn file_link() -> Url {
    Url::parse(&format!("https://mega.nz/file/{FILE}#{KEY}")).unwrap()
}

fn folder_link() -> Url {
    Url::parse(&format!("https://mega.nz/folder/{FOLDER}#{FOLDER_KEY}")).unwrap()
}

/// A request the stand-in read.
#[derive(Debug, Clone)]
struct Logged {
    target: String,
    content_type: Option<String>,
    body: String,
}

#[derive(Default)]
struct State {
    log: Vec<Logged>,
    /// Errors MEGA answers commands with, by command, instead of the
    /// normal answer.
    errors: HashMap<&'static str, i64>,
}

struct Server {
    addr: SocketAddr,
    state: Arc<Mutex<State>>,
}

impl Server {
    async fn start() -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(State::default()));
        let encrypted = {
            let mut data = plain();
            FileKey::from_raw(&raw_key()).decryptor(0).apply(&mut data);
            Arc::new(data)
        };
        let shared = state.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let (state, encrypted) = (shared.clone(), encrypted.clone());
                tokio::spawn(serve(stream, addr, state, encrypted));
            }
        });
        Self { addr, state }
    }

    fn base(&self) -> String {
        format!("http://{}/", self.addr)
    }

    fn error(&self, command: &'static str, code: i64) {
        self.state.lock().unwrap().errors.insert(command, code);
    }

    fn log(&self) -> Vec<Logged> {
        self.state.lock().unwrap().log.clone()
    }
}

fn raw_key() -> [u8; 32] {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(KEY)
        .unwrap()
        .try_into()
        .unwrap()
}

async fn serve(
    mut stream: TcpStream,
    addr: SocketAddr,
    state: Arc<Mutex<State>>,
    encrypted: Arc<Vec<u8>>,
) {
    let mut buf = Vec::new();
    let head_end = loop {
        let mut chunk = [0; 4096];
        let n = stream.read(&mut chunk).await.unwrap();
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.lines();
    let mut first = lines.next().unwrap().split(' ');
    let (method, target) = (
        first.next().unwrap().to_owned(),
        first.next().unwrap().to_owned(),
    );
    let headers: HashMap<String, String> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let length: usize = headers
        .get("content-length")
        .map_or(0, |v| v.parse().unwrap());
    while buf.len() < head_end + length {
        let mut chunk = [0; 4096];
        let n = stream.read(&mut chunk).await.unwrap();
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = String::from_utf8_lossy(&buf[head_end..head_end + length]).to_string();
    state.lock().unwrap().log.push(Logged {
        target: target.clone(),
        content_type: headers.get("content-type").cloned(),
        body: body.clone(),
    });
    let url = Url::parse(&format!("http://{addr}{target}")).unwrap();
    let response = if method == "POST" && url.path() == "/cs" {
        let folder = url
            .query_pairs()
            .find(|(k, _)| k == "n")
            .map(|(_, v)| v.into_owned());
        let commands: Vec<Value> = serde_json::from_str(&body).unwrap();
        let answer = command(&commands[0], folder.as_deref(), addr, &state);
        let text = json!([answer]).to_string();
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{text}",
            text.len()
        )
        .into_bytes()
    } else if url.path() == "/dl/file" {
        {
            let start: usize = headers
                .get("range")
                .and_then(|r| r.strip_prefix("bytes="))
                .and_then(|r| r.strip_suffix('-'))
                .map_or(0, |s| s.parse().unwrap());
            let status = if start > 0 {
                "206 Partial Content"
            } else {
                "200 OK"
            };
            let mut out = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/octet-stream\r\n\
                 Content-Length: {}\r\n{}Connection: close\r\n\r\n",
                SIZE - start,
                if start > 0 {
                    format!("Content-Range: bytes {start}-{}/{SIZE}\r\n", SIZE - 1)
                } else {
                    String::new()
                }
            )
            .into_bytes();
            out.extend_from_slice(&encrypted[start..]);
            out
        }
    } else {
        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
    };
    let _ = stream.write_all(&response).await;
    let _ = stream.shutdown().await;
}

/// MEGA's answer to one command.
fn command(cmd: &Value, folder: Option<&str>, addr: SocketAddr, state: &Mutex<State>) -> Value {
    let a = cmd["a"].as_str().unwrap();
    let errors = state.lock().unwrap().errors.clone();
    match a {
        "g" => {
            if let Some(code) = errors.get("g") {
                return json!(code);
            }
            let known = match folder {
                None => cmd["p"] == FILE,
                Some(f) => f == FOLDER && cmd["n"] == "Ch1ld_aa",
            };
            if !known {
                return json!(-9);
            }
            let mut answer = json!({ "s": SIZE, "at": ATTRIBUTES });
            if cmd["g"] == 1 {
                assert_eq!(cmd["ssl"], 2, "HTTPS download URLs are asked for");
                answer["g"] = json!(format!("http://{addr}/dl/file"));
            }
            answer
        }
        "f" => {
            if let Some(code) = errors.get("f") {
                return json!(code);
            }
            assert_eq!(folder, Some(FOLDER));
            json!({ "f": [
                { "h": FOLDER, "p": "Outside1", "t": 1, "a": "x", "k": "FoLd3r_x:x" },
                { "h": "Ch1ld_aa", "p": FOLDER, "t": 0, "s": SIZE, "a": ATTRIBUTES,
                  "k": format!("{FOLDER}:{NODE_KEY}") },
                { "h": "Sub_f0ld", "p": FOLDER, "t": 1, "a": "x", "k": "FoLd3r_x:x" },
                { "h": "Ch2ld_bb", "p": "Sub_f0ld", "t": 0, "s": SIZE, "a": ATTRIBUTES,
                  "k": format!("{FOLDER}:{NODE_KEY}") },
                // Not below the shared folder.
                { "h": "Elsewher", "p": "Outside1", "t": 0, "s": 1, "a": "x", "k": "a:b" },
            ]})
        }
        other => panic!("unexpected command {other}"),
    }
}

fn transport() -> Arc<HttpTransport> {
    let mut config = HttpConfig::default();
    config
        .egress
        .allowed_networks
        .push("127.0.0.1/32".parse().unwrap());
    Arc::new(HttpTransport::new(config).unwrap())
}

fn mega(server: &Server) -> Mega {
    Mega::new(transport(), Endpoint::custom(&server.base()).unwrap())
}

/// Downloads `hosted` from `from` through the transport, decrypted.
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
    let mut decoder = hosted.encoding.as_ref().unwrap().decoder(from);
    let mut out = Vec::new();
    while let Some(chunk) = response.body.next_chunk().await.unwrap() {
        let mut chunk = chunk.to_vec();
        decoder.decode(&mut chunk);
        out.extend_from_slice(&chunk);
    }
    out
}

#[tokio::test]
async fn a_file_link_is_checked_without_its_key_leaving_the_machine() {
    let server = Server::start().await;
    let mega = mega(&server);
    assert!(mega.recognizes(&file_link()));
    assert_eq!(
        mega.check(&file_link()).await,
        HostCheck::File {
            name: Some("sample.bin".into()),
            size: Some(SIZE as u64),
        }
    );
    let log = server.log();
    assert_eq!(log.len(), 1);
    assert!(log[0].target.starts_with("/cs?id="));
    assert_eq!(log[0].content_type.as_deref(), Some("application/json"));
    assert_eq!(
        serde_json::from_str::<Value>(&log[0].body).unwrap(),
        json!([{ "a": "g", "p": FILE }])
    );
    assert!(!log[0].body.contains(KEY) && !log[0].target.contains(KEY));
}

#[tokio::test]
async fn a_file_downloads_decrypted_from_any_offset_and_passes_its_mac() {
    let server = Server::start().await;
    let hosted = mega(&server).resolve(&file_link()).await.unwrap();
    assert_eq!(hosted.size, SIZE as u64);
    assert_eq!(hosted.name.as_deref(), Some("sample.bin"));
    assert!(hosted.identity.starts_with(&format!("{FILE};{SIZE};")));
    assert!(!hosted.identity.contains(KEY));
    assert!(
        !format!("{hosted:?}").contains("/dl/file"),
        "the URL never prints"
    );
    let whole = download(&hosted, 0).await;
    assert!(whole == plain());
    let mut check = hosted.encoding.as_ref().unwrap().verifier();
    check.update(&whole);
    assert!(check.verify());
    let tail = download(&hosted, 1_000_003).await;
    assert!(tail == plain()[1_000_003..]);
    // Every resolve is a fresh request for a URL; the identity is stable.
    let again = mega(&server).resolve(&file_link()).await.unwrap();
    assert_eq!(again.identity, hosted.identity);
}

#[tokio::test]
async fn a_folder_lists_every_file_below_it_flattened() {
    let server = Server::start().await;
    let mega = mega(&server);
    let HostCheck::Folder(files) = mega.check(&folder_link()).await else {
        panic!("not listed as a folder");
    };
    let expected: Vec<Url> = ["Ch1ld_aa", "Ch2ld_bb"]
        .iter()
        .map(|h| {
            Url::parse(&format!(
                "https://mega.nz/folder/{FOLDER}#{FOLDER_KEY}/file/{h}"
            ))
            .unwrap()
        })
        .collect();
    assert_eq!(files, expected);
    assert!(files.iter().all(|f| mega.recognizes(f)));
    // A subfolder link lists only what is below it.
    let sub = Url::parse(&format!(
        "https://mega.nz/folder/{FOLDER}#{FOLDER_KEY}/folder/Sub_f0ld"
    ))
    .unwrap();
    assert_eq!(
        mega.check(&sub).await,
        HostCheck::Folder(vec![expected[1].clone()])
    );
    // A folder cannot be downloaded as one file.
    let refused = mega.resolve(&folder_link()).await.unwrap_err();
    assert_eq!(refused.code, FailureCode::UnsupportedCapability);
}

#[tokio::test]
async fn a_file_in_a_folder_downloads_with_the_key_from_the_listing() {
    let server = Server::start().await;
    let mega = mega(&server);
    let child = MegaLink::in_folder(
        &link::handle(FOLDER),
        &link::folder_key(),
        &link::handle("Ch1ld_aa"),
    );
    let hosted = mega.resolve(&child).await.unwrap();
    assert_eq!(hosted.name.as_deref(), Some("sample.bin"));
    assert!(hosted.identity.starts_with(&format!("{FOLDER}/Ch1ld_aa;")));
    assert!(download(&hosted, 0).await == plain());
    let downloads: Vec<_> = server
        .log()
        .into_iter()
        .filter(|l| l.body.contains("\"g\""))
        .collect();
    assert!(downloads[0].target.ends_with(&format!("&n={FOLDER}")));
    assert_eq!(
        serde_json::from_str::<Value>(&downloads[0].body).unwrap(),
        json!([{ "a": "g", "g": 1, "n": "Ch1ld_aa", "ssl": 2 }])
    );
}

/// Building folder links from parts.
mod link {
    use wayfetch_adapters::mega::link::{FolderKey, Handle, MegaLink};

    pub fn handle(raw: &str) -> Handle {
        Handle::new(raw).unwrap()
    }

    pub fn folder_key() -> FolderKey {
        match MegaLink::parse(&super::folder_link()).unwrap() {
            MegaLink::Folder { key, .. } => key,
            MegaLink::File { .. } => unreachable!(),
        }
    }
}

#[tokio::test]
async fn megas_errors_have_their_meaning() {
    let server = Server::start().await;
    let mega = mega(&server);
    let gone = Url::parse(&format!("https://mega.nz/file/Gone_123#{KEY}")).unwrap();
    assert!(matches!(mega.check(&gone).await, HostCheck::Offline(_)));
    assert_eq!(
        mega.resolve(&gone).await.unwrap_err().code,
        FailureCode::ResourceDeleted
    );
    server.error("g", -16);
    assert!(matches!(
        mega.check(&file_link()).await,
        HostCheck::Offline(_)
    ));
    server.error("g", -17);
    assert_eq!(
        mega.resolve(&file_link()).await.unwrap_err().code,
        FailureCode::QuotaExhausted
    );
    server.error("g", -4);
    let limited = mega.resolve(&file_link()).await.unwrap_err();
    assert_eq!(limited.code, FailureCode::HostUnavailableViaProvider);
    assert!(limited.retry_after.is_some());
    assert!(matches!(
        mega.check(&file_link()).await,
        HostCheck::Unknown(CheckReason::RateLimited, _)
    ));
    server.error("f", -9);
    assert!(matches!(
        mega.check(&folder_link()).await,
        HostCheck::Offline(_)
    ));
    // Storage's refusal once the transfer quota is used up.
    assert_eq!(
        mega.refused(509).map(|e| e.code),
        Some(FailureCode::QuotaExhausted)
    );
    assert_eq!(mega.refused(404), None);
}

#[tokio::test]
async fn an_unreachable_api_leaves_the_check_unknown() {
    let mega = Mega::new(
        transport(),
        Endpoint::custom("http://127.0.0.1:9/").unwrap(),
    );
    assert!(matches!(
        mega.check(&file_link()).await,
        HostCheck::Unknown(CheckReason::Unreachable, _)
    ));
    assert_eq!(
        mega.resolve(&file_link()).await.unwrap_err().code,
        FailureCode::HostUnavailableViaProvider
    );
}
