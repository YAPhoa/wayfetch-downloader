//! A local stand-in for Real-Debrid's API over plain HTTP on `127.0.0.1`,
//! following the documented device flow and `GET /rest/1.0/user`. Every
//! value it hands out is synthetic; each answer can be overridden to
//! misbehave on purpose, and every request is logged.

#![allow(dead_code, reason = "each test file uses part of the fixture")]

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A request as the fixture read it.
#[derive(Debug, Clone)]
pub struct Logged {
    pub method: String,
    /// The path without the query.
    pub path: String,
    pub query: HashMap<String, String>,
    pub authorization: Option<String>,
    pub form: HashMap<String, String>,
}

/// A canned answer.
#[derive(Debug, Clone)]
pub enum Answer {
    Json(u16, String),
    /// Bytes written verbatim, then the connection closes.
    Raw(Vec<u8>),
    /// Never answers.
    Hang,
}

impl Answer {
    pub fn json(status: u16, body: impl Into<String>) -> Self {
        Self::Json(status, body.into())
    }
}

/// The fixture's account and behavior.
#[derive(Debug)]
pub struct Rd {
    /// Polls answered "pending" before the person approves.
    pub pending_polls: usize,
    /// Access tokens issued so far.
    pub issued: usize,
    /// Whether the current device code was exchanged for tokens.
    pub device_code_used: bool,
    /// The access token currently accepted, if any.
    pub valid_access: Option<String>,
    /// The refresh token currently accepted.
    pub valid_refresh: Option<String>,
    /// Lifetime of issued access tokens, in seconds.
    pub token_lifetime: u64,
    /// The account `/user` describes.
    pub user: String,
    /// Answers that replace the normal one, by path, used once each.
    pub once: HashMap<&'static str, Vec<Answer>>,
    /// Answers that replace the normal one, by path, every time.
    pub always: HashMap<&'static str, Answer>,
    pub log: Vec<Logged>,
}

pub const CLIENT_ID: &str = "synthetic-user-client-id";
pub const CLIENT_SECRET: &str = "synthetic-user-client-secret";
pub const DEVICE_CODE: &str = "synthetic-device-code";
pub const USER_CODE: &str = "ABCD-1234";

pub const DEVICE_PATH: &str = "/oauth/v2/device/code";
pub const CREDENTIALS_PATH: &str = "/oauth/v2/device/credentials";
pub const TOKEN_PATH: &str = "/oauth/v2/token";
pub const USER_PATH: &str = "/rest/1.0/user";

pub fn premium_user(seconds: i64) -> String {
    format!(
        r#"{{"id":4242,"username":"synthetic-user","email":"synthetic@example.invalid","points":0,"locale":"en","avatar":"","type":"premium","premium":{seconds},"expiration":"2030-01-01T00:00:00.000Z"}}"#
    )
}

impl Default for Rd {
    fn default() -> Self {
        Self {
            pending_polls: 1,
            issued: 0,
            device_code_used: false,
            valid_access: None,
            valid_refresh: None,
            token_lifetime: 3600,
            user: premium_user(86_400),
            once: HashMap::new(),
            always: HashMap::new(),
            log: Vec::new(),
        }
    }
}

impl Rd {
    fn answer(&mut self, request: &Logged, origin: &str) -> Answer {
        if let Some(queue) = self.once.get_mut(request.path.as_str())
            && !queue.is_empty()
        {
            return queue.remove(0);
        }
        if let Some(answer) = self.always.get(request.path.as_str()) {
            return answer.clone();
        }
        let q = |name: &str| request.query.get(name).map(String::as_str);
        let f = |name: &str| request.form.get(name).map(String::as_str);
        match request.path.as_str() {
            DEVICE_PATH => {
                if q("client_id") != Some(wayfetch_adapters::real_debrid::OPEN_SOURCE_CLIENT_ID)
                    || q("new_credentials") != Some("yes")
                {
                    return Answer::json(400, r#"{"error":"bad_request","error_code":2}"#);
                }
                self.device_code_used = false;
                Answer::json(
                    200,
                    format!(
                        r#"{{"device_code":"{DEVICE_CODE}","user_code":"{USER_CODE}","interval":1,"expires_in":600,"verification_url":"{origin}/device","direct_verification_url":"{origin}/device?code={USER_CODE}"}}"#
                    ),
                )
            }
            CREDENTIALS_PATH => {
                if q("code") != Some(DEVICE_CODE) {
                    return Answer::json(403, r#"{"error":"bad_code","error_code":1}"#);
                }
                if self.pending_polls > 0 {
                    self.pending_polls -= 1;
                    return Answer::json(
                        403,
                        r#"{"error":"authorization_pending","error_code":1}"#,
                    );
                }
                Answer::json(
                    200,
                    format!(r#"{{"client_id":"{CLIENT_ID}","client_secret":"{CLIENT_SECRET}"}}"#),
                )
            }
            TOKEN_PATH => {
                let code = f("code").unwrap_or_default().to_owned();
                let accepted = f("client_id") == Some(CLIENT_ID)
                    && f("client_secret") == Some(CLIENT_SECRET)
                    && f("grant_type") == Some("http://oauth.net/grant_type/device/1.0")
                    && (code == DEVICE_CODE && !self.device_code_used
                        || self.valid_refresh.as_deref() == Some(code.as_str()));
                if !accepted {
                    return Answer::json(400, r#"{"error":"invalid_grant","error_code":9}"#);
                }
                if code == DEVICE_CODE {
                    self.device_code_used = true;
                }
                self.issued += 1;
                let access = format!("synthetic-access-{}", self.issued);
                let refresh = format!("synthetic-refresh-{}", self.issued);
                self.valid_access = Some(access.clone());
                self.valid_refresh = Some(refresh.clone());
                Answer::json(
                    200,
                    format!(
                        r#"{{"access_token":"{access}","expires_in":{},"token_type":"Bearer","refresh_token":"{refresh}"}}"#,
                        self.token_lifetime
                    ),
                )
            }
            USER_PATH => {
                let accepted = match (&request.authorization, &self.valid_access) {
                    (Some(header), Some(token)) => header == &format!("Bearer {token}"),
                    _ => false,
                };
                if accepted {
                    Answer::json(200, self.user.clone())
                } else {
                    Answer::json(401, r#"{"error":"bad_token","error_code":8}"#)
                }
            }
            _ => Answer::json(404, r#"{"error":"unknown_ressource","error_code":7}"#),
        }
    }
}

/// A running fixture.
pub struct Server {
    addr: SocketAddr,
    pub state: Arc<Mutex<Rd>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Server {
    pub async fn start() -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(Rd::default()));
        let task = {
            let state = state.clone();
            let origin = format!("http://{addr}");
            tokio::spawn(async move {
                while let Ok((socket, _)) = listener.accept().await {
                    tokio::spawn(serve(socket, state.clone(), origin.clone()));
                }
            })
        };
        Self { addr, state, task }
    }

    /// The API base address.
    pub fn base(&self) -> String {
        format!("http://{}/", self.addr)
    }

    pub fn with<T>(&self, change: impl FnOnce(&mut Rd) -> T) -> T {
        change(&mut self.state.lock().unwrap())
    }

    pub fn log(&self) -> Vec<Logged> {
        self.with(|rd| rd.log.clone())
    }

    pub fn hits(&self, path: &str) -> usize {
        self.log().iter().filter(|r| r.path == path).count()
    }
}

fn pairs(text: &str) -> HashMap<String, String> {
    url::form_urlencoded::parse(text.as_bytes())
        .into_owned()
        .collect()
}

async fn serve(mut socket: TcpStream, state: Arc<Mutex<Rd>>, origin: String) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let (head_len, logged, length) = loop {
        let Ok(n) = socket.read(&mut chunk).await else {
            return;
        };
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);
        let mut headers = [httparse::EMPTY_HEADER; 32];
        let mut parsed = httparse::Request::new(&mut headers);
        if let Ok(httparse::Status::Complete(len)) = parsed.parse(&buf) {
            let target = parsed.path.unwrap_or("/");
            let (path, query) = target.split_once('?').unwrap_or((target, ""));
            let header = |name: &str| {
                parsed
                    .headers
                    .iter()
                    .find(|h| h.name.eq_ignore_ascii_case(name))
                    .map(|h| String::from_utf8_lossy(h.value).into_owned())
            };
            let length = header("content-length").map_or(0, |v| v.parse().unwrap());
            let logged = Logged {
                method: parsed.method.unwrap_or_default().to_owned(),
                path: path.to_owned(),
                query: pairs(query),
                authorization: header("authorization"),
                form: HashMap::new(),
            };
            break (len, logged, length);
        }
    };
    while buf.len() < head_len + length {
        match socket.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let mut logged = logged;
    logged.form = pairs(&String::from_utf8_lossy(&buf[head_len..head_len + length]));
    let answer = {
        let mut rd = state.lock().unwrap();
        rd.log.push(logged.clone());
        rd.answer(&logged, &origin)
    };
    match answer {
        Answer::Json(status, body) => {
            let head = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(body.as_bytes()).await;
        }
        Answer::Raw(bytes) => {
            let _ = socket.write_all(&bytes).await;
        }
        Answer::Hang => std::future::pending().await,
    }
    let _ = socket.shutdown().await;
}
