//! The `wayfetch` executable driven as a subprocess against the local
//! HTTP/HTTPS fixture: submit and publish, observe durable progress, pause,
//! resume, and cancel, shut down mid-transfer and finish after a restart,
//! refuse a second owner, survive malformed input, and keep TLS
//! verification and the egress policy unless the operator names an
//! exception.
//!
//! Sessions run with `--json`, except where the text output is the subject,
//! so each test reads typed results instead of scraping text. Every file is
//! large enough to cross a default 8 MiB checkpoint before the fixture
//! stalls it, so there are durable bytes to observe and resume from.

#[path = "../../engine/tests/support/mod.rs"]
mod engine_support;
#[path = "../../net/tests/support/mod.rs"]
mod support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use bytes::Bytes;
use engine_support::body;
use serde_json::Value;
use support::{Resource, Server};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::mpsc;

const BIN: &str = env!("CARGO_BIN_EXE_wayfetch");
/// How long any one wait for the session may take.
const DEADLINE: Duration = Duration::from_secs(30);
const MIB: usize = 1024 * 1024;
/// The engine's default checkpoint interval.
const CHECKPOINT: u64 = 8 * MIB as u64;
/// Size of the files served, and where transfers stall: past two
/// checkpoints.
const SIZE: usize = 20 * MIB + 11;
const STALL: usize = 17 * MIB + 5;
/// Durable bytes once a transfer stalled at [`STALL`] has settled.
const SETTLED: u64 = 2 * CHECKPOINT;

/// A state directory and an existing, uninitialized download root.
struct Dirs {
    tmp: tempfile::TempDir,
    state: PathBuf,
    root: PathBuf,
}

impl Dirs {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let (state, root) = (tmp.path().join("state"), tmp.path().join("downloads"));
        std::fs::create_dir(&root).unwrap();
        Self { tmp, state, root }
    }

    /// Initialized, with `server`'s CA saved for `--ca-cert`.
    fn ready(server: &Server) -> Self {
        let dirs = Self::new();
        let out = dirs.init_root();
        assert!(out.status.success(), "{out:?}");
        std::fs::write(dirs.ca(), server.ca()).unwrap();
        dirs
    }

    fn ca(&self) -> PathBuf {
        self.tmp.path().join("ca.der")
    }

    fn init_root(&self) -> std::process::Output {
        std::process::Command::new(BIN)
            .arg("init-root")
            .arg("--state-dir")
            .arg(&self.state)
            .arg(&self.root)
            .output()
            .unwrap()
    }

    /// Arguments that let a session reach and trust the local fixture.
    fn fixture_args(&self) -> Vec<String> {
        vec![
            "--allow-network".into(),
            "127.0.0.1/32".into(),
            "--ca-cert".into(),
            self.ca().to_str().unwrap().into(),
        ]
    }

    fn published(&self, name: &str) -> Vec<u8> {
        std::fs::read(self.root.join(name)).unwrap()
    }

    /// Lengths of the partials in the root's staging directory.
    fn partials(&self) -> Vec<u64> {
        std::fs::read_dir(self.root.join(".wayfetch/staging"))
            .unwrap()
            .map(|e| e.unwrap().metadata().unwrap().len())
            .collect()
    }

    /// Every file under the state directory with its contents.
    fn state_snapshot(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut files = BTreeMap::new();
        for entry in std::fs::read_dir(&self.state).unwrap() {
            let path = entry.unwrap().path();
            if path.is_file() {
                files.insert(path.clone(), std::fs::read(&path).unwrap());
            }
        }
        files
    }
}

/// A running `wayfetch run` process.
struct Session {
    process: tokio::process::Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::UnboundedReceiver<String>,
    /// Every line of standard output read so far.
    transcript: Vec<String>,
    stderr: tokio::task::JoinHandle<String>,
}

/// How a session ended.
struct Finished {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

impl Session {
    /// Starts a JSON session on `dirs` with `args`.
    async fn json(dirs: &Dirs, args: &[String]) -> Self {
        let mut session = Self::spawn(dirs, args, true);
        session.until(|v| v["type"] == "ready").await;
        session
    }

    fn spawn(dirs: &Dirs, args: &[String], json: bool) -> Self {
        let mut process = run_command(dirs, args, json).spawn().unwrap();
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
        Self {
            stdin: process.stdin.take(),
            process,
            lines,
            transcript: Vec::new(),
            stderr,
        }
    }

    async fn send(&mut self, line: impl AsRef<[u8]>) {
        let stdin = self.stdin.as_mut().unwrap();
        stdin.write_all(line.as_ref()).await.unwrap();
        stdin.write_all(b"\n").await.unwrap();
        stdin.flush().await.unwrap();
    }

    /// Reads output until a line satisfies `done`, and returns it.
    async fn until(&mut self, done: impl Fn(&Value) -> bool) -> Value {
        let deadline = tokio::time::Instant::now() + DEADLINE;
        loop {
            let line = tokio::time::timeout_at(deadline, self.lines.recv())
                .await
                .unwrap_or_else(|_| panic!("timed out; output so far:\n{}", self.transcript()))
                .unwrap_or_else(|| panic!("the session exited:\n{}", self.transcript()));
            self.transcript.push(line.clone());
            let value: Value = serde_json::from_str(&line).unwrap();
            if done(&value) {
                return value;
            }
        }
    }

    fn transcript(&self) -> String {
        self.transcript.join("\n")
    }

    /// Waits for an event saying job `id` reached `stage`.
    async fn stage(&mut self, id: &str, stage: &str) -> Value {
        self.until(|v| {
            let Some(job) = event_job(v).filter(|job| job["id"] == id) else {
                return false;
            };
            let parked = job["stage"] == "failed";
            assert!(job["stage"] == stage || !parked, "{job}");
            job["stage"] == stage
        })
        .await
        .pointer("/event/kind/job")
        .unwrap()
        .clone()
    }

    async fn inspect(&mut self, id: &str) -> Value {
        self.send(format!("inspect {id}")).await;
        self.until(|v| v["type"] == "job" && v["job"]["id"] == id)
            .await["job"]
            .clone()
    }

    /// Polls `inspect` until the job's durable bytes settle at a stall.
    async fn settled(&mut self, id: &str) -> u64 {
        let deadline = tokio::time::Instant::now() + DEADLINE;
        loop {
            let done = self.inspect(id).await["bytes_done"].as_u64().unwrap();
            if done >= SETTLED {
                assert!(done <= STALL as u64, "{done}");
                return done;
            }
            assert!(tokio::time::Instant::now() < deadline, "durable {done}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Submits `url` and returns the new job's ID.
    async fn submit(&mut self, url: &str) -> String {
        self.send(url).await;
        self.until(|v| v["type"] == "submitted").await;
        let added = self
            .until(|v| v.pointer("/event/kind/type") == Some(&"job_added".into()))
            .await;
        added
            .pointer("/event/kind/job/id")
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned()
    }

    /// Closes standard input and waits for the process to exit.
    async fn finish(mut self) -> Finished {
        drop(self.stdin.take());
        let status = tokio::time::timeout(DEADLINE, self.process.wait())
            .await
            .expect("the session exits")
            .unwrap();
        while let Some(line) = self.lines.recv().await {
            self.transcript.push(line);
        }
        Finished {
            status,
            stdout: self.transcript(),
            stderr: self.stderr.await.unwrap(),
        }
    }
}

/// `wayfetch run` on `dirs` with every standard stream piped.
fn run_command(dirs: &Dirs, args: &[String], json: bool) -> Command {
    let mut command = Command::new(BIN);
    command
        .arg("run")
        .arg("--state-dir")
        .arg(&dirs.state)
        .arg("--download-root")
        .arg(&dirs.root)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if json {
        command.arg("--json");
    }
    command
}

/// Sends `signal` (as `kill` names it) to process `pid`.
#[cfg(unix)]
fn signal(pid: u32, signal: &str) {
    let status = std::process::Command::new("kill")
        .args([&format!("-{signal}"), &pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success());
}

/// The job in a `job_added` or `job_changed` event.
fn event_job(v: &Value) -> Option<&Value> {
    (v["type"] == "event")
        .then(|| v.pointer("/event/kind/job"))
        .flatten()
}

/// Checks that the session shut down cleanly for `reason`.
fn assert_clean_stop(finished: &Finished, reason: &str) {
    assert!(finished.status.success(), "{}", finished.stderr);
    let lines: Vec<Value> = finished
        .stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert!(
        lines
            .iter()
            .any(|v| v["type"] == "shutting_down" && v["reason"] == reason),
        "{}",
        finished.stdout
    );
    let stopped = lines.last().unwrap();
    assert_eq!(stopped["type"], "stopped");
    assert_eq!(stopped["clean"], true);
    assert_eq!(stopped["state_dir_released"], true);
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

#[tokio::test(flavor = "multi_thread")]
async fn a_download_shows_progress_and_publishes_the_exact_bytes() {
    let server = Server::https().await;
    let dirs = Dirs::ready(&server);
    // A signed URL: its signature must never be printed.
    let path = "/big.bin?signature=s3cr3t-signature";
    let data = stalled(&server, path);
    let mut session = Session::json(&dirs, &dirs.fixture_args()).await;
    let id = session.submit(&server.url(path)).await;
    assert_eq!(id, "job-1");

    // Durable progress is visible while the transfer runs.
    let durable = session.settled(&id).await;
    session.send("list").await;
    let row = session.until(|v| v["type"] == "job").await;
    assert_eq!(row["job"]["stage"], "transferring");
    assert_eq!(row["job"]["bytes_done"], durable);
    assert_eq!(row["job"]["bytes_total"], SIZE);
    assert_eq!(session.until(|v| v["type"] == "listed").await["count"], 1);

    release(&server, path);
    let done = session.stage(&id, "complete").await;
    assert_eq!(done["artifact"]["path"], "big.bin");
    assert_eq!(done["artifact"]["size"], SIZE);
    assert_eq!(dirs.published("big.bin"), data);

    session.send("quit").await;
    let finished = session.finish().await;
    assert_clean_stop(&finished, "quit");
    for output in [&finished.stdout, &finished.stderr] {
        assert!(!output.contains("s3cr3t"), "{output}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pause_resume_and_cancel_keep_partial_data() {
    let server = Server::https().await;
    let dirs = Dirs::ready(&server);
    let data = stalled(&server, "/p.bin");
    let mut session = Session::json(&dirs, &dirs.fixture_args()).await;
    let paused = session.submit(&server.url("/p.bin")).await;
    let settled = session.settled(&paused).await;

    session.send(format!("pause {paused}")).await;
    let updated = session.until(|v| v["type"] == "updated").await;
    assert_eq!(updated["changed"], true);
    // Stopping may commit the bytes written since the last checkpoint.
    let job = session.stage(&paused, "paused").await;
    let durable = job["bytes_done"].as_u64().unwrap();
    assert!((settled..=STALL as u64).contains(&durable), "{job}");
    assert_eq!(dirs.partials(), [STALL as u64]);

    // Resuming continues from the durable bytes.
    release(&server, "/p.bin");
    session.send("resume 1").await;
    session.until(|v| v["type"] == "updated").await;
    session.stage(&paused, "complete").await;
    assert_eq!(dirs.published("p.bin"), data);
    assert_eq!(server.ranges("/p.bin"), [None, Some((durable, None))]);

    stalled(&server, "/c.bin");
    let cancelled = session.submit(&server.url("/c.bin")).await;
    let durable = session.settled(&cancelled).await;
    session.send(format!("cancel {cancelled}")).await;
    session.until(|v| v["type"] == "updated").await;
    session.stage(&cancelled, "cancelled").await;
    // End of input shuts down; the cancelled job keeps its partial.
    let finished = session.finish().await;
    assert_clean_stop(&finished, "end of input");
    let [partial] = dirs.partials()[..] else {
        panic!("{:?}", dirs.partials())
    };
    assert!(partial >= durable, "{partial} < {durable}");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn ctrl_c_during_a_transfer_keeps_the_job_for_the_next_start() {
    let server = Server::https().await;
    let dirs = Dirs::ready(&server);
    let data = stalled(&server, "/r.bin");
    let mut session = Session::json(&dirs, &dirs.fixture_args()).await;
    let id = session.submit(&server.url("/r.bin")).await;
    let settled = session.settled(&id).await;

    signal(session.process.id().unwrap(), "INT");
    assert_clean_stop(&session.finish().await, "interrupted");

    // The next start takes the job over and asks only for the rest.
    release(&server, "/r.bin");
    let mut session = Session::json(&dirs, &dirs.fixture_args()).await;
    let done = session.stage(&id, "complete").await;
    assert!(done["generation"].as_u64().unwrap() > 1, "{done}");
    assert_eq!(dirs.published("r.bin"), data);
    // Shutdown may have committed the bytes written since the last
    // checkpoint; the resume starts at whatever was durable.
    let ranges = server.ranges("/r.bin");
    let [None, Some((from, None))] = ranges[..] else {
        panic!("{ranges:?}")
    };
    assert!((settled..=STALL as u64).contains(&from), "{from}");
    assert_eq!(server.log()[1].header("if-range"), Some("\"v1\""));
    session.send("quit").await;
    assert_clean_stop(&session.finish().await, "quit");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_owner_is_refused_and_changes_nothing() {
    let server = Server::https().await;
    let dirs = Dirs::ready(&server);
    let mut session = Session::json(&dirs, &dirs.fixture_args()).await;
    session.send("list").await;
    session.until(|v| v["type"] == "listed").await;
    let before = dirs.state_snapshot();

    let second = Session::spawn(&dirs, &dirs.fixture_args(), true)
        .finish()
        .await;
    assert!(!second.status.success());
    assert!(
        second.stderr.contains("owned by another running engine"),
        "{}",
        second.stderr
    );
    assert_eq!(second.stdout, "");
    let init = dirs.init_root();
    assert!(!init.status.success());
    assert_eq!(dirs.state_snapshot(), before);

    // The owner carries on.
    let data = body(MIB);
    server.put("/s.bin", Resource::new(data.clone()));
    let id = session.submit(&server.url("/s.bin")).await;
    session.stage(&id, "complete").await;
    assert_eq!(dirs.published("s.bin"), data);
    session.send("quit").await;
    assert_clean_stop(&session.finish().await, "quit");
}

#[tokio::test(flavor = "multi_thread")]
async fn malformed_and_oversized_input_is_rejected_without_ending_the_session() {
    let server = Server::https().await;
    let dirs = Dirs::ready(&server);
    let mut session = Session::spawn(&dirs, &dirs.fixture_args(), false);
    let bad: Vec<Vec<u8>> = vec![
        vec![b'a'; 64 * 1024],
        b"\xff\xfe not utf-8".to_vec(),
        b"\x00\x01\x02\x1b[2J".to_vec(),
        b"frobnicate".to_vec(),
        b"add".to_vec(),
        b"pause".to_vec(),
        b"pause job-x".to_vec(),
        b"pause 1 2".to_vec(),
        b"inspect 999".to_vec(),
        b"resume 18446744073709551616".to_vec(),
        b"ftp://example.com/file".to_vec(),
        b"https://user:hunter2@example.com/file".to_vec(),
        format!("https://example.com/{}", "a".repeat(9000)).into_bytes(),
        "http://a.b/ ".repeat(1001).into_bytes(),
    ];
    for line in &bad {
        session.send(line).await;
    }
    session.send("list").await;
    session.send("help").await;
    session.send("quit").await;
    let finished = session.finish().await;
    assert!(finished.status.success(), "{}", finished.stderr);
    assert!(finished.stdout.contains("0 job(s)"), "{}", finished.stdout);
    assert!(finished.stdout.contains("commands:"), "{}", finished.stdout);
    assert!(
        finished
            .stdout
            .ends_with("stopped; unfinished jobs continue on the next start")
    );
    let errors = finished.stderr.lines().filter(|l| l.starts_with("error: "));
    assert_eq!(errors.count(), bad.len(), "{}", finished.stderr);
    for echoed in ["hunter2", "frobnicate", "aaaa", "ftp:"] {
        assert!(!finished.stderr.contains(echoed), "{}", finished.stderr);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn tls_verification_and_the_egress_policy_hold_without_explicit_exceptions() {
    let server = Server::https().await;
    let dirs = Dirs::ready(&server);
    server.put("/t.bin", Resource::new(body(1024)));

    // Production defaults: the local server is not a permitted destination,
    // so nothing reaches it.
    let mut session = Session::json(&dirs, &[]).await;
    let id = session.submit(&server.url("/t.bin")).await;
    let failed = session.stage(&id, "failed").await;
    assert_eq!(failed["failure"]["code"], "unsupported_capability");
    assert!(server.log().is_empty());
    session.send("quit").await;
    assert_clean_stop(&session.finish().await, "quit");

    // The network allowed but its CA not trusted: the handshake fails
    // before any request is sent.
    let args = ["--allow-network".to_owned(), "127.0.0.1/32".to_owned()];
    let mut session = Session::json(&dirs, &args).await;
    let id = session.submit(&server.url("/t.bin")).await;
    let failed = session.stage(&id, "failed").await;
    assert_eq!(failed["failure"]["code"], "unsupported_capability");
    let message = failed["failure"]["message"].as_str().unwrap();
    assert!(message.contains("UnknownIssuer"), "{message}");
    assert!(server.log().is_empty());
    session.send("quit").await;
    assert_clean_stop(&session.finish().await, "quit");

    // No option turns verification off.
    let help = std::process::Command::new(BIN)
        .args(["run", "--help"])
        .output()
        .unwrap();
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("--ca-cert") && help.contains("cannot be turned off"));
    for option in ["insecure", "no-verify", "danger"] {
        assert!(!help.contains(option), "{help}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_never_initializes_its_download_root() {
    let server = Server::https().await;
    let dirs = Dirs::new();
    std::fs::write(dirs.ca(), server.ca()).unwrap();
    server.put("/i.bin", Resource::new(body(1024)));

    let mut session = Session::spawn(&dirs, &dirs.fixture_args(), true);
    let warning = session.until(|v| v["type"] == "root_unavailable").await;
    assert!(warning["reason"].is_string());
    let id = session.submit(&server.url("/i.bin")).await;
    let blocked = session.stage(&id, "failed").await;
    assert_eq!(blocked["failure"]["code"], "destination_unavailable");
    session.send("quit").await;
    assert_clean_stop(&session.finish().await, "quit");
    assert!(!dirs.root.join(".wayfetch").exists());
    assert!(empty(&dirs.root));

    // The operator initializes it explicitly.
    let out = dirs.init_root();
    assert!(out.status.success(), "{out:?}");
    assert!(dirs.root.join(".wayfetch").is_dir());
}

fn empty(dir: &Path) -> bool {
    std::fs::read_dir(dir).unwrap().next().is_none()
}

/// A reader that stopped reading must not keep the session from stopping:
/// with standard output (JSON) or standard error (text errors) full and
/// never drained, a signal still shuts the engine down and the process
/// exits.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_stalled_reader_cannot_block_shutdown() {
    // Each line answers with output on the stream under test: `help` on
    // standard output, an unknown command's error on standard error.
    let cases = [(true, "help", "INT"), (false, "frobnicate", "TERM")];
    for (json, line, sig) in cases {
        let dirs = Dirs::new();
        assert!(dirs.init_root().status.success());
        let mut process = run_command(&dirs, &[], json).spawn().unwrap();
        // Held open and never read.
        let (_stdout, _stderr) = (process.stdout.take(), process.stderr.take());
        let mut stdin = process.stdin.take().unwrap();
        // Far more output than a pipe and the output queue hold. Before
        // the fix, the session stopped reading input once its write
        // blocked, so these writes may stall too.
        let input = format!("{line}\n").repeat(20_000);
        let _ =
            tokio::time::timeout(Duration::from_secs(5), stdin.write_all(input.as_bytes())).await;
        tokio::time::sleep(Duration::from_millis(200)).await;

        signal(process.id().unwrap(), sig);
        let status = tokio::time::timeout(DEADLINE, process.wait())
            .await
            .unwrap_or_else(|_| panic!("{sig} with a stalled reader did not stop the session"))
            .unwrap();
        assert!(status.success(), "{sig}: {status:?}");
        // The engine released its state directory: a new owner may start.
        drop(stdin);
        let next = Session::spawn(&dirs, &[], true).finish().await;
        assert!(next.status.success(), "{}", next.stderr);
    }
}

/// A reader that closes standard output ends the session as soon as a
/// write fails, without waiting for more input; standard input stays open.
#[tokio::test(flavor = "multi_thread")]
async fn closing_standard_output_ends_the_session() {
    let dirs = Dirs::new();
    assert!(dirs.init_root().status.success());
    let mut process = run_command(&dirs, &[], true).spawn().unwrap();
    let mut stdout = BufReader::new(process.stdout.take().unwrap()).lines();
    let ready = tokio::time::timeout(DEADLINE, stdout.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(ready.contains(r#""type":"ready""#), "{ready}");
    drop(stdout);
    // One command whose answer can no longer be written.
    let mut stdin = process.stdin.take().unwrap();
    stdin.write_all(b"help\n").await.unwrap();
    stdin.flush().await.unwrap();

    tokio::time::timeout(DEADLINE, process.wait())
        .await
        .expect("the session exits with standard input still open")
        .unwrap();
    // The engine released its state directory.
    let next = Session::spawn(&dirs, &[], true).finish().await;
    assert!(next.status.success(), "{}", next.stderr);
    drop(stdin);
}
