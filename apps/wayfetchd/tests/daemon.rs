//! The `wayfetchd` executable driven as a subprocess over its HTTP API,
//! against the local HTTPS fixture: the complete submit, list, inspect,
//! pause, resume, and cancel workflow; authentication and scoped tokens;
//! server-sent events that resume from `Last-Event-ID` or say to
//! resynchronize; downloads that continue after every client disconnects;
//! request and stream bounds; and graceful shutdown on SIGTERM or SIGINT
//! with jobs kept for the next start and a single owner per state
//! directory.
//!
//! Files that stall are large enough to cross a default 8 MiB checkpoint
//! first, so there are durable bytes to observe and resume from.

#[path = "../../../crates/engine/tests/support/mod.rs"]
mod engine_support;
#[path = "../../../crates/net/tests/support/mod.rs"]
mod support;

use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use bytes::Bytes;
use engine_support::body;
use eventsource_stream::{Event, Eventsource};
use futures_util::{Stream, StreamExt};
use reqwest::StatusCode;
use serde_json::{Value, json};
use support::{Resource, Server};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

const BIN: &str = env!("CARGO_BIN_EXE_wayfetchd");
/// How long any one wait may take.
const DEADLINE: Duration = Duration::from_secs(30);
const MIB: usize = 1024 * 1024;
/// The engine's default checkpoint interval.
const CHECKPOINT: u64 = 8 * MIB as u64;
/// Size of stalling files, and where they stall: past two checkpoints.
const SIZE: usize = 20 * MIB + 11;
const STALL: usize = 17 * MIB + 5;
/// Durable bytes once a transfer stalled at [`STALL`] has settled.
const SETTLED: u64 = 2 * CHECKPOINT;
/// A token for every permission, and one that may only read.
const OWNER_TOKEN: &str = "owner-0123456789abcdef0123456789abcdef";
const READER_TOKEN: &str = "reader-0123456789abcdef0123456789abcdef";
/// The API's default bounds.
const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_EVENT_STREAMS: usize = 16;

/// A state directory, a download root, a token file, and the fixture's CA.
struct Dirs {
    tmp: tempfile::TempDir,
    state: PathBuf,
    root: PathBuf,
    tokens: PathBuf,
}

impl Dirs {
    /// An uninitialized root and no token file.
    fn new(server: &Server) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Self {
            state: tmp.path().join("state"),
            root: tmp.path().join("downloads"),
            tokens: tmp.path().join("tokens"),
            tmp,
        };
        std::fs::create_dir(&dirs.root).unwrap();
        std::fs::write(dirs.ca(), server.ca()).unwrap();
        dirs
    }

    /// An initialized root and a token file with [`OWNER_TOKEN`] and
    /// [`READER_TOKEN`].
    fn ready(server: &Server) -> Self {
        let dirs = Self::new(server);
        let out = std::process::Command::new(BIN)
            .arg("init-root")
            .arg("--state-dir")
            .arg(&dirs.state)
            .arg(&dirs.root)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        write_private(
            &dirs.tokens,
            &format!("{OWNER_TOKEN} read,submit,control\n{READER_TOKEN} read\n"),
        );
        dirs
    }

    fn ca(&self) -> PathBuf {
        self.tmp.path().join("ca.der")
    }

    fn published(&self, name: &str) -> Vec<u8> {
        std::fs::read(self.root.join(name)).unwrap()
    }
}

fn write_private(path: &std::path::Path, text: &str) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(text.as_bytes()).unwrap();
}

/// A running `wayfetchd run --json`.
struct Daemon {
    process: tokio::process::Child,
    lines: mpsc::UnboundedReceiver<String>,
    transcript: Vec<String>,
    stderr: tokio::task::JoinHandle<String>,
    base: String,
    instance: String,
}

/// How a daemon ended.
struct Finished {
    status: ExitStatus,
    lines: Vec<Value>,
    stderr: String,
}

impl Daemon {
    /// Starts a daemon on `dirs` and waits until it serves.
    async fn start(dirs: &Dirs) -> Self {
        let mut process = Self::command(dirs).spawn().unwrap();
        let mut out = BufReader::new(process.stdout.take().unwrap()).lines();
        let (send, lines) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok(Some(line)) = out.next_line().await {
                let _ = send.send(line);
            }
        });
        let mut err = process.stderr.take().unwrap();
        let stderr = tokio::spawn(async move {
            let mut text = String::new();
            let _ = err.read_to_string(&mut text).await;
            text
        });
        let mut daemon = Self {
            process,
            lines,
            transcript: Vec::new(),
            stderr,
            base: String::new(),
            instance: String::new(),
        };
        let listening = daemon.until(|v| v["type"] == "listening").await;
        daemon.base = format!("http://{}", listening["address"].as_str().unwrap());
        daemon.instance = listening["instance"].as_str().unwrap().to_owned();
        daemon
    }

    fn command(dirs: &Dirs) -> Command {
        let mut command = Command::new(BIN);
        command
            .arg("run")
            .arg("--state-dir")
            .arg(&dirs.state)
            .arg("--download-root")
            .arg(&dirs.root)
            .arg("--token-file")
            .arg(&dirs.tokens)
            .args(["--listen", "127.0.0.1:0", "--json"])
            .args(["--allow-network", "127.0.0.1/32", "--ca-cert"])
            .arg(dirs.ca())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }

    /// Reads output until a line satisfies `done`, and returns it.
    async fn until(&mut self, done: impl Fn(&Value) -> bool) -> Value {
        let deadline = tokio::time::Instant::now() + DEADLINE;
        loop {
            let line = tokio::time::timeout_at(deadline, self.lines.recv())
                .await
                .unwrap_or_else(|_| panic!("timed out; output:\n{}", self.transcript.join("\n")))
                .unwrap_or_else(|| panic!("the daemon exited:\n{}", self.transcript.join("\n")));
            self.transcript.push(line.clone());
            let value: Value = serde_json::from_str(&line).unwrap();
            if done(&value) {
                return value;
            }
        }
    }

    /// A client using `token`.
    fn client(&self, token: &str) -> Client {
        // The workspace builds reqwest without a default TLS provider; a
        // plain-HTTP client still needs one installed.
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
        Client {
            http: reqwest::Client::builder().no_proxy().build().unwrap(),
            base: self.base.clone(),
            token: Some(token.to_owned()),
            instance: self.instance.clone(),
        }
    }

    fn owner(&self) -> Client {
        self.client(OWNER_TOKEN)
    }

    fn signal(&self, signal: &str) {
        let pid = self.process.id().unwrap().to_string();
        let status = std::process::Command::new("kill")
            .args([&format!("-{signal}"), &pid])
            .status()
            .unwrap();
        assert!(status.success());
    }

    /// Waits for the process to exit.
    async fn finish(mut self) -> Finished {
        let status = tokio::time::timeout(DEADLINE, self.process.wait())
            .await
            .expect("the daemon exits")
            .unwrap();
        while let Some(line) = self.lines.recv().await {
            self.transcript.push(line);
        }
        Finished {
            status,
            lines: self
                .transcript
                .iter()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect(),
            stderr: self.stderr.await.unwrap(),
        }
    }

    /// Sends `signal` and checks the daemon stopped cleanly.
    async fn stop(self, signal: &str) {
        self.signal(signal);
        let finished = self.finish().await;
        assert!(finished.status.success(), "{}", finished.stderr);
        let stopped = finished.lines.last().unwrap();
        assert_eq!(stopped["type"], "stopped", "{stopped}");
        assert_eq!(stopped["clean"], true, "{stopped}");
        assert_eq!(stopped["state_dir_released"], true, "{stopped}");
    }
}

/// An HTTP client of one daemon.
#[derive(Clone)]
struct Client {
    http: reqwest::Client,
    base: String,
    token: Option<String>,
    instance: String,
}

impl Client {
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let request = self.http.request(method, format!("{}{path}", self.base));
        match &self.token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> (StatusCode, Value) {
        let response = request.send().await.unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    async fn get(&self, path: &str) -> (StatusCode, Value) {
        self.send(self.request(reqwest::Method::GET, path)).await
    }

    async fn post(&self, path: &str, body: &Value) -> (StatusCode, Value) {
        self.send(self.request(reqwest::Method::POST, path).json(body))
            .await
    }

    /// A command body for this daemon's instance with a fresh key.
    fn command(&self, fields: Value) -> Value {
        let mut body = json!({
            "idempotency_key": fresh_key(),
            "target": self.instance,
        });
        body.as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        body
    }

    /// Submits `url` and returns the new job's ID.
    async fn submit(&self, url: &str) -> String {
        let known = self.jobs().await.len();
        let (status, result) = self
            .post("/v1/inputs", &self.command(json!({"items": [url]})))
            .await;
        assert_eq!(status, StatusCode::OK, "{result}");
        assert_eq!(result["type"], "submitted", "{result}");
        let deadline = tokio::time::Instant::now() + DEADLINE;
        loop {
            let jobs = self.jobs().await;
            if jobs.len() > known {
                return jobs[known]["id"].as_str().unwrap().to_owned();
            }
            assert!(tokio::time::Instant::now() < deadline, "no job added");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Every job, a page at a time.
    async fn jobs(&self) -> Vec<Value> {
        let mut jobs = Vec::new();
        let mut path = "/v1/jobs?page_size=2".to_owned();
        loop {
            let (status, page) = self.get(&path).await;
            assert_eq!(status, StatusCode::OK, "{page}");
            assert_eq!(page["type"], "jobs", "{page}");
            jobs.extend(page["items"].as_array().unwrap().iter().cloned());
            match page["next_after"].as_str() {
                Some(after) => path = format!("/v1/jobs?page_size=2&after={after}"),
                None => return jobs,
            }
        }
    }

    async fn job(&self, id: &str) -> Value {
        let (status, result) = self.get(&format!("/v1/jobs/{id}")).await;
        assert_eq!(status, StatusCode::OK, "{result}");
        result["job"].clone()
    }

    /// Polls job `id` until `done` holds.
    async fn wait(&self, id: &str, done: impl Fn(&Value) -> bool) -> Value {
        let deadline = tokio::time::Instant::now() + DEADLINE;
        loop {
            let job = self.job(id).await;
            if done(&job) {
                return job;
            }
            let parked = matches!(job["stage"].as_str(), Some("failed" | "blocked"));
            assert!(!parked, "{job}");
            assert!(tokio::time::Instant::now() < deadline, "{job}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn stage(&self, id: &str, stage: &str) -> Value {
        self.wait(id, |job| job["stage"] == stage).await
    }

    /// Waits until a stalled transfer's durable bytes settle.
    async fn settled(&self, id: &str) -> u64 {
        let job = self
            .wait(id, |job| job["bytes_done"].as_u64().unwrap() >= SETTLED)
            .await;
        let done = job["bytes_done"].as_u64().unwrap();
        assert!(done <= STALL as u64, "{done}");
        done
    }

    /// Sends `action` for job `id` at its current revision.
    async fn control(&self, id: &str, action: &str) -> (StatusCode, Value) {
        let revision = self.job(id).await["revision"].clone();
        let body = self.command(json!({"action": action, "expected_revision": revision}));
        self.post(&format!("/v1/jobs/{id}/commands"), &body).await
    }

    /// Opens the event stream, resuming after `last_event_id` if given.
    async fn events(&self, last_event_id: Option<&str>) -> Events {
        let mut request = self.request(reqwest::Method::GET, "/v1/events");
        if let Some(id) = last_event_id {
            request = request.header("last-event-id", id);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()["content-type"],
            "text/event-stream",
            "{response:?}"
        );
        Events(Box::pin(response.bytes_stream().eventsource()))
    }
}

type EventResult = Result<Event, eventsource_stream::EventStreamError<reqwest::Error>>;

/// An open event stream.
struct Events(std::pin::Pin<Box<dyn Stream<Item = EventResult> + Send>>);

impl Events {
    /// The next event, or `None` once the stream ended.
    async fn next(&mut self) -> Option<Event> {
        tokio::time::timeout(DEADLINE, self.0.next())
            .await
            .expect("an event in time")
            .map(|e| e.unwrap())
    }

    /// Reads until an event satisfies `done`, and returns it with its data.
    async fn until(&mut self, done: impl Fn(&Event, &Value) -> bool) -> (Event, Value) {
        loop {
            let event = self.next().await.expect("the stream continues");
            let data: Value = serde_json::from_str(&event.data).unwrap();
            if done(&event, &data) {
                return (event, data);
            }
        }
    }

    /// Reads until job `id` reaches `stage`; returns the event.
    async fn stage(&mut self, id: &str, stage: &str) -> Event {
        self.until(|e, data| {
            e.event == "event"
                && data.pointer("/kind/job/id") == Some(&id.into())
                && data.pointer("/kind/job/stage") == Some(&stage.into())
        })
        .await
        .0
    }
}

fn fresh_key() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// A file served at `path`, stalled at [`STALL`] until released.
fn stalled(server: &Server, path: &str) -> Bytes {
    let data = body(SIZE);
    server.put(
        path,
        Resource {
            stall_at: Some(STALL),
            ..Resource::new(data.clone())
        },
    );
    data
}

fn release(server: &Server, path: &str) {
    server.update(path, |r| r.stall_at = None);
}

fn position(id: &str) -> u64 {
    id.rsplit_once(':').unwrap().1.parse().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_complete_workflow_over_http() {
    let server = Server::https().await;
    let dirs = Dirs::ready(&server);
    let daemon = Daemon::start(&dirs).await;
    let client = daemon.owner();

    let (status, hello) = client.get("/v1/capabilities").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(hello["instance"], daemon.instance.as_str());
    assert_eq!(hello["limits"]["max_submit_items"], 1000);

    // Submit, inspect, list, and publish the exact bytes.
    let small = body(MIB);
    server.put("/small.bin", Resource::new(small.clone()));
    let first = client.submit(&server.url("/small.bin")).await;
    let done = client.stage(&first, "complete").await;
    assert_eq!(done["artifact"]["path"], "small.bin");
    assert_eq!(dirs.published("small.bin"), small);

    // Pause and resume keep partial data; a stale revision conflicts.
    let data = stalled(&server, "/big.bin");
    let second = client.submit(&server.url("/big.bin")).await;
    client.settled(&second).await;
    let stale = client.job(&second).await["revision"].clone();
    let (status, paused) = client.control(&second, "pause").await;
    assert_eq!(status, StatusCode::OK, "{paused}");
    assert_eq!(paused["type"], "job_updated");
    assert_eq!(paused["changed"], true);
    let job = client.stage(&second, "paused").await;
    assert!(job["bytes_done"].as_u64().unwrap() >= SETTLED, "{job}");
    let conflict = client.command(json!({"action": "resume", "expected_revision": stale}));
    let (status, error) = client
        .post(&format!("/v1/jobs/{second}/commands"), &conflict)
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(error["code"], "revision_conflict");
    assert_eq!(error["current_revision"], job["revision"]);

    // A retried command replays its result; a reused key with another
    // command, or another instance, is refused.
    let resume = client.command(json!({
        "action": "resume",
        "expected_revision": job["revision"],
    }));
    let path = format!("/v1/jobs/{second}/commands");
    let (status, resumed) = client.post(&path, &resume).await;
    assert_eq!(status, StatusCode::OK, "{resumed}");
    assert_eq!(client.post(&path, &resume).await, (status, resumed));
    let mut reused = resume.clone();
    reused["action"] = "cancel".into();
    let (status, error) = client.post(&path, &reused).await;
    assert_eq!(
        (status, &error["code"]),
        (StatusCode::CONFLICT, &"idempotency_conflict".into())
    );
    let mut elsewhere = client.command(json!({"action": "pause", "expected_revision": 1}));
    elsewhere["target"] = "00000000-0000-4000-8000-000000000000".into();
    let (status, error) = client.post(&path, &elsewhere).await;
    assert_eq!(
        (status, &error["code"]),
        (StatusCode::CONFLICT, &"wrong_instance".into())
    );

    release(&server, "/big.bin");
    client.stage(&second, "complete").await;
    assert_eq!(dirs.published("big.bin"), data);

    // Cancel keeps the partial too.
    stalled(&server, "/third.bin");
    let third = client.submit(&server.url("/third.bin")).await;
    client.settled(&third).await;
    let (status, _) = client.control(&third, "cancel").await;
    assert_eq!(status, StatusCode::OK);
    let cancelled = client.stage(&third, "cancelled").await;
    assert!(cancelled["bytes_done"].as_u64().unwrap() >= SETTLED);

    // Pages and filters.
    let ids: Vec<_> = client
        .jobs()
        .await
        .iter()
        .map(|j| j["id"].clone())
        .collect();
    assert_eq!(ids, [first.as_str(), second.as_str(), third.as_str()]);
    let (_, page) = client.get("/v1/jobs?stage=cancelled,paused").await;
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["items"][0]["id"], third.as_str());
    let (status, error) = client.get("/v1/jobs?page_size=501").await;
    assert_eq!(
        (status, &error["code"]),
        (StatusCode::PAYLOAD_TOO_LARGE, &"limit_exceeded".into())
    );
    let (status, _) = client.get("/v1/jobs?stage=sleeping").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, error) = client.get("/v1/jobs/job-99").await;
    assert_eq!(
        (status, &error["code"]),
        (StatusCode::NOT_FOUND, &"not_found".into())
    );

    daemon.stop("INT").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn requests_without_a_valid_token_are_refused() {
    let server = Server::https().await;
    let dirs = Dirs::ready(&server);
    let daemon = Daemon::start(&dirs).await;
    let owner = daemon.owner();
    server.put("/a.bin", Resource::new(body(1024)));
    let job = owner.submit(&server.url("/a.bin")).await;
    let revision = owner.stage(&job, "complete").await["revision"].clone();

    let routes = [
        (reqwest::Method::GET, "/v1/capabilities".to_owned()),
        (reqwest::Method::GET, "/v1/jobs".to_owned()),
        (reqwest::Method::GET, format!("/v1/jobs/{job}")),
        (reqwest::Method::GET, "/v1/events".to_owned()),
        (reqwest::Method::POST, "/v1/inputs".to_owned()),
        (reqwest::Method::POST, format!("/v1/jobs/{job}/commands")),
    ];
    let submit = owner.command(json!({"items": [server.url("/a.bin")]}));
    let control = owner.command(json!({"action": "pause", "expected_revision": revision}));
    let mut wrong = owner.clone();
    wrong.token = Some(format!("{OWNER_TOKEN}x"));
    let mut none = owner.clone();
    none.token = None;
    for (method, path) in &routes {
        let body = if path.ends_with("inputs") {
            &submit
        } else {
            &control
        };
        for client in [&none, &wrong] {
            let response = client
                .request(method.clone(), path)
                .json(body)
                .send()
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{method} {path}"
            );
            assert_eq!(response.headers()["www-authenticate"], "Bearer");
        }
        // Not a bearer token.
        let response = none
            .request(method.clone(), path)
            .basic_auth("owner", Some(OWNER_TOKEN))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    // A read-only token reads but cannot change anything.
    let reader = daemon.client(READER_TOKEN);
    assert_eq!(reader.jobs().await.len(), 1);
    reader
        .events(None)
        .await
        .until(|e, _| e.event == "hello")
        .await;
    let (status, error) = reader.post("/v1/inputs", &submit).await;
    assert_eq!(
        (status, &error["code"]),
        (StatusCode::FORBIDDEN, &"permission_denied".into())
    );
    let (status, _) = reader
        .post(&format!("/v1/jobs/{job}/commands"), &control)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(owner.jobs().await.len(), 1);
    assert_eq!(owner.job(&job).await["revision"], revision);

    daemon.stop("TERM").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_token_file_is_created_owner_only_and_never_printed() {
    use std::os::unix::fs::PermissionsExt;
    let server = Server::https().await;
    let dirs = Dirs::new(&server);
    let daemon = Daemon::start(&dirs).await;
    // The root was never initialized, and the daemon does not do it.
    assert!(
        daemon
            .transcript
            .iter()
            .any(|l| l.contains("root_unavailable"))
    );
    assert_eq!(std::fs::read_dir(&dirs.root).unwrap().count(), 0);
    assert_eq!(
        serde_json::from_str::<Value>(&daemon.transcript[0]).unwrap()["type"],
        "token_file_created"
    );

    let mode = std::fs::metadata(&dirs.tokens)
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    let text = std::fs::read_to_string(&dirs.tokens).unwrap();
    let line = text.lines().find(|l| !l.starts_with('#')).unwrap();
    let (token, scopes) = line.split_once(' ').unwrap();
    assert_eq!(scopes, "read,submit,control");
    let (status, _) = daemon.client(token).get("/v1/capabilities").await;
    assert_eq!(status, StatusCode::OK);

    daemon.signal("TERM");
    let finished = daemon.finish().await;
    assert!(finished.status.success(), "{}", finished.stderr);
    let everything = format!("{:?}{}", finished.lines, finished.stderr);
    assert!(!everything.contains(token));
}

#[tokio::test(flavor = "multi_thread")]
async fn event_streams_resume_from_the_last_event_id_or_say_to_resync() {
    let server = Server::https().await;
    let dirs = Dirs::ready(&server);
    let daemon = Daemon::start(&dirs).await;
    let client = daemon.owner();

    let mut events = client.events(None).await;
    let (hello, data) = events.until(|_, _| true).await;
    assert_eq!(hello.event, "hello");
    assert_eq!(data["instance"], daemon.instance.as_str());
    assert_eq!(hello.id, "", "a hello is not a resumable position");

    server.put("/a.bin", Resource::new(body(1024)));
    let first = client.submit(&server.url("/a.bin")).await;
    let complete = events.stage(&first, "complete").await;
    let seen = complete.id.clone();
    assert_eq!(seen, format!("{}:{}", daemon.instance, position(&seen)));
    drop(events);

    // Changes while disconnected.
    server.put("/b.bin", Resource::new(body(2048)));
    let second = client.submit(&server.url("/b.bin")).await;
    client.stage(&second, "complete").await;

    // Reconnecting with the last ID replays exactly what was missed.
    let mut events = client.events(Some(&seen)).await;
    assert_eq!(events.next().await.unwrap().event, "hello");
    let mut expected = position(&seen) + 1;
    let mut replayed = Vec::new();
    loop {
        let (event, data) = events.until(|_, _| true).await;
        assert_eq!(event.event, "event");
        assert_eq!(position(&event.id), expected, "no gap and no repeat");
        expected += 1;
        replayed.push(data["kind"].clone());
        if data.pointer("/kind/job/id") == Some(&second.as_str().into())
            && data.pointer("/kind/job/stage") == Some(&"complete".into())
        {
            break;
        }
    }
    assert_eq!(replayed[0]["type"], "batch_accepted");
    assert!(
        replayed
            .iter()
            .all(|k| k.pointer("/job/id") != Some(&first.as_str().into()))
    );

    // A query parameter works as well, and live events follow the replay.
    let (_, page) = client.get("/v1/jobs").await;
    let as_of = format!("{}:{}", daemon.instance, page["as_of"]["position"]);
    let mut live = client.events(None).await;
    live.next().await;
    let response = client
        .request(reqwest::Method::GET, &format!("/v1/events?after={as_of}"))
        .send()
        .await
        .unwrap();
    let mut after = Events(Box::pin(response.bytes_stream().eventsource()));
    assert_eq!(after.next().await.unwrap().event, "hello");
    server.put("/c.bin", Resource::new(body(10)));
    let third = client.submit(&server.url("/c.bin")).await;
    after.stage(&third, "complete").await;
    live.stage(&third, "complete").await;

    // A cursor this instance cannot honour says to resynchronize and ends.
    for (cursor, reason) in [
        (
            "00000000-0000-4000-8000-000000000000:1".to_owned(),
            "unknown_instance",
        ),
        (format!("{}:999999", daemon.instance), "cursor_ahead"),
    ] {
        let mut stream = client.events(Some(&cursor)).await;
        assert_eq!(stream.next().await.unwrap().event, "hello");
        let (event, data) = stream.until(|_, _| true).await;
        assert_eq!(
            (event.event.as_str(), &data["reason"]),
            ("resync", &reason.into())
        );
        assert!(stream.next().await.is_none());
    }
    let (status, _) = client.get("/v1/events?after=garbage").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    daemon.stop("TERM").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn downloads_continue_after_every_client_disconnects() {
    let server = Server::https().await;
    let dirs = Dirs::ready(&server);
    let daemon = Daemon::start(&dirs).await;
    let data = stalled(&server, "/r.bin");
    let id = {
        let client = daemon.owner();
        let mut events = client.events(None).await;
        events.next().await;
        let id = client.submit(&server.url("/r.bin")).await;
        events.stage(&id, "transferring").await;
        client.settled(&id).await;
        // Every connection closes mid-transfer: the stream, the client's
        // pooled connections, a request cut off in its headers, and one
        // whose response is never read.
        let address = daemon.base.trim_start_matches("http://");
        for request in [
            format!("GET /v1/jobs HTTP/1.1\r\nAuthorization: Bearer {OWNER_TOKEN}\r\n"),
            format!("GET /v1/events HTTP/1.1\r\nAuthorization: Bearer {OWNER_TOKEN}\r\n\r\n"),
        ] {
            let mut tcp = tokio::net::TcpStream::connect(address).await.unwrap();
            tcp.write_all(request.as_bytes()).await.unwrap();
        }
        drop(events);
        drop(client);
        id
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    release(&server, "/r.bin");

    let client = daemon.owner();
    let done = client.stage(&id, "complete").await;
    assert_eq!(done["generation"], 1, "the transfer was never restarted");
    assert_eq!(dirs.published("r.bin"), data);
    assert_eq!(server.ranges("/r.bin"), [None]);
    daemon.stop("TERM").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn requests_and_streams_are_bounded() {
    let server = Server::https().await;
    let dirs = Dirs::ready(&server);
    let daemon = Daemon::start(&dirs).await;
    let client = daemon.owner();

    // An oversized body is refused before it is read whole.
    let padding = "x".repeat(MAX_BODY_BYTES);
    let huge = client.command(json!({"items": [format!("https://example.test/{padding}")]}));
    let (status, error) = client.post("/v1/inputs", &huge).await;
    assert_eq!(
        (status, &error["code"]),
        (StatusCode::PAYLOAD_TOO_LARGE, &"limit_exceeded".into())
    );
    // Malformed and unknown fields are refused without echoing them.
    let mut extra = client.command(json!({"items": ["https://example.test/a"]}));
    extra["is_admin"] = true.into();
    let (status, error) = client.post("/v1/inputs", &extra).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(!error["message"].as_str().unwrap().contains("is_admin"));
    let bad = client.command(json!({"items": ["https://example.test/a", "ftp://SECRET/"]}));
    let (status, error) = client.post("/v1/inputs", &bad).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(error["message"].as_str().unwrap().contains("item 2"));
    assert!(!error["message"].as_str().unwrap().contains("SECRET"));
    let many: Vec<_> = (0..1001).map(|i| format!("https://e.test/{i}")).collect();
    let (status, error) = client
        .post("/v1/inputs", &client.command(json!({"items": many})))
        .await;
    assert_eq!(
        (status, &error["code"]),
        (StatusCode::PAYLOAD_TOO_LARGE, &"limit_exceeded".into())
    );
    assert!(client.jobs().await.is_empty());

    // Event streams are capped; closing one frees its place.
    let mut streams = Vec::new();
    for _ in 0..MAX_EVENT_STREAMS {
        let mut stream = client.events(None).await;
        stream.next().await;
        streams.push(stream);
    }
    let (status, error) = client.get("/v1/events").await;
    assert_eq!(
        (status, &error["code"]),
        (StatusCode::SERVICE_UNAVAILABLE, &"overloaded".into())
    );
    // Commands still get through while streams are at the cap.
    assert_eq!(client.get("/v1/capabilities").await.0, StatusCode::OK);
    drop(streams.pop());
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        let response = client
            .request(reqwest::Method::GET, "/v1/events")
            .send()
            .await
            .unwrap();
        if response.status() == StatusCode::OK {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Shutdown ends open streams instead of waiting on them.
    daemon.stop("TERM").await;
    for mut stream in streams {
        assert!(stream.next().await.is_none());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn sigterm_keeps_jobs_for_the_next_start_and_one_daemon_owns_the_state() {
    let server = Server::https().await;
    let dirs = Dirs::ready(&server);
    let data = stalled(&server, "/r.bin");
    let daemon = Daemon::start(&dirs).await;
    let client = daemon.owner();
    let id = client.submit(&server.url("/r.bin")).await;
    let settled = client.settled(&id).await;

    // A second daemon on the same state directory is refused.
    let second = Daemon::command(&dirs).output().await.unwrap();
    assert!(!second.status.success());
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        stderr.contains("owned by another running engine"),
        "{stderr}"
    );

    // An open stream does not hold up shutdown, and shutdown cancels
    // nothing.
    let mut events = client.events(None).await;
    events.next().await;
    daemon.stop("TERM").await;
    assert!(events.next().await.is_none());

    release(&server, "/r.bin");
    let daemon = Daemon::start(&dirs).await;
    let client = daemon.owner();
    let done = client.stage(&id, "complete").await;
    assert!(done["generation"].as_u64().unwrap() > 1, "{done}");
    assert_eq!(dirs.published("r.bin"), data);
    let ranges = server.ranges("/r.bin");
    let [None, Some((from, None))] = ranges[..] else {
        panic!("{ranges:?}")
    };
    assert!((settled..=STALL as u64).contains(&from), "{from}");
    daemon.stop("TERM").await;
}
