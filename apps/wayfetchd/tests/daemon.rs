//! The `wayfetchd` executable driven as a subprocess over its HTTP API,
//! against the local HTTPS fixture: the bundled web UI; the complete
//! submit, list, inspect, pause, resume, and cancel workflow; choosing the
//! default destination among the server's folders;
//! authentication and scoped tokens; server-sent events that resume from
//! `Last-Event-ID` or say to resynchronize; downloads that continue after
//! every client disconnects; request and stream bounds; graceful shutdown
//! on SIGTERM or SIGINT with jobs kept for the next start and a single
//! owner per state directory; and browser access: owner setup with a setup
//! secret, sign-in, sessions for commands and events, origin and CSRF
//! checks, sign-out, restarts, and owner reset.
//!
//! Files that stall are large enough to cross a default 8 MiB checkpoint
//! first, so there are durable bytes to observe and resume from.

mod common;
#[path = "../../../crates/engine/tests/support/mod.rs"]
mod engine_support;
#[path = "../../../crates/net/tests/support/mod.rs"]
mod support;

use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use common::{
    Client, DEADLINE, Events, MIB, OWNER_TOKEN, SETTLED, STALL, release, stalled, write_private,
};
use engine_support::body;
use eventsource_stream::Eventsource;
use reqwest::StatusCode;
use serde_json::{Value, json};
use support::{Resource, Server};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

const BIN: &str = env!("CARGO_BIN_EXE_wayfetchd");
/// A token that may only read.
const READER_TOKEN: &str = "reader-0123456789abcdef0123456789abcdef";
/// The API's default bounds.
const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_EVENT_STREAMS: usize = 16;
const MAX_CONNECTIONS: usize = 64;
/// The API's deadlines for request headers and for a body after them.
const READ_DEADLINE: Duration = Duration::from_secs(10);

/// A state directory, a download root, a token file, and the fixture's CA,
/// and with browser access, an auth directory.
struct Dirs {
    tmp: tempfile::TempDir,
    state: PathBuf,
    root: PathBuf,
    /// Further roots, passed with `--root`.
    roots: Vec<PathBuf>,
    tokens: PathBuf,
    browser: bool,
}

impl Dirs {
    /// An uninitialized root and no token file.
    fn new(server: &Server) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Self {
            state: tmp.path().join("state"),
            root: tmp.path().join("downloads"),
            roots: Vec::new(),
            tokens: tmp.path().join("tokens"),
            browser: false,
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
        dirs.init_root();
        write_private(
            &dirs.tokens,
            &format!("{OWNER_TOKEN} read,submit,control,sources\n{READER_TOKEN} read\n"),
        );
        dirs
    }

    fn init_root(&self) {
        self.init(&self.root);
    }

    /// Runs `init-root` for `root`.
    fn init(&self, root: &std::path::Path) {
        let out = std::process::Command::new(BIN)
            .arg("init-root")
            .arg("--state-dir")
            .arg(&self.state)
            .arg(root)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
    }

    fn ca(&self) -> PathBuf {
        self.tmp.path().join("ca.der")
    }

    /// The auth directory, for browser access over loopback HTTP.
    fn auth(&self) -> PathBuf {
        self.tmp.path().join("auth")
    }

    fn published(&self, name: &str) -> Vec<u8> {
        std::fs::read(self.root.join(name)).unwrap()
    }
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
        Self::command_on(dirs, "127.0.0.1:0")
    }

    /// The command for a daemon listening on `listen`.
    fn command_on(dirs: &Dirs, listen: &str) -> Command {
        let mut command = Command::new(BIN);
        command
            .arg("run")
            .arg("--state-dir")
            .arg(&dirs.state)
            .arg("--download-root")
            .arg(&dirs.root)
            .arg("--token-file")
            .arg(&dirs.tokens)
            .args(["--listen", listen, "--json"])
            .args(["--allow-network", "127.0.0.1/32", "--ca-cert"])
            .arg(dirs.ca());
        for root in &dirs.roots {
            command.arg("--root").arg(root);
        }
        if dirs.browser {
            command
                .arg("--auth-dir")
                .arg(dirs.auth())
                .arg("--dev-insecure-loopback");
        }
        command
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
        Client::new(&self.base, &self.instance, Some(token))
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
    client.serves_the_ui().await;

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
    // Jobs name the host of their link, never the link itself.
    assert_eq!(page["items"][0]["original_host"], "127.0.0.1");

    // Counts by stage, with the same filters.
    let (status, counts) = client.get("/v1/job-counts").await;
    assert_eq!(status, StatusCode::OK, "{counts}");
    assert_eq!(counts["type"], "job_counts");
    assert_eq!(counts["stages"], json!({"complete": 2, "cancelled": 1}));
    let (_, counts) = client.get("/v1/job-counts?stage=cancelled,paused").await;
    assert_eq!(counts["stages"], json!({"cancelled": 1}));
    let (status, _) = client.get("/v1/job-counts?stage=sleeping").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = client.get("/v1/job-counts?after=job-1").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, error) = client.get("/v1/jobs/job-99").await;
    assert_eq!(
        (status, &error["code"]),
        (StatusCode::NOT_FOUND, &"not_found".into())
    );

    daemon.stop("INT").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_default_destination_is_chosen_among_server_folders() {
    let server = Server::https().await;
    let mut dirs = Dirs::ready(&server);
    let media = dirs.tmp.path().join("media");
    std::fs::create_dir(&media).unwrap();
    dirs.init(&media);
    dirs.roots.push(media.clone());
    let daemon = Daemon::start(&dirs).await;
    let client = daemon.owner();

    // Readers see the folders as paths on the server.
    let (status, settings) = daemon.client(READER_TOKEN).get("/v1/settings").await;
    assert_eq!(status, StatusCode::OK, "{settings}");
    assert_eq!(settings["type"], "settings");
    let roots = settings["roots"].as_array().unwrap();
    let paths: Vec<_> = roots.iter().map(|r| r["path"].as_str().unwrap()).collect();
    assert_eq!(
        paths,
        [dirs.root.to_str().unwrap(), media.to_str().unwrap()]
    );
    assert!(roots.iter().all(|r| r["available"] == true), "{settings}");
    assert_eq!(settings["default_root"], roots[0]["id"]);
    let media_id = roots[1]["id"].clone();

    let choose = |revision: &Value, root: &Value| {
        client.command(json!({"expected_revision": revision, "default_root": root}))
    };
    let change = choose(&settings["revision"], &media_id);
    let (status, _) = daemon
        .client(READER_TOKEN)
        .put("/v1/settings", &change)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, updated) = client.put("/v1/settings", &change).await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["type"], "settings_updated");
    assert_eq!(updated["changed"], true);
    assert_eq!(
        client.put("/v1/settings", &change).await,
        (status, updated.clone())
    );
    let (status, error) = client
        .put(
            "/v1/settings",
            &choose(&updated["revision"], &"root-99".into()),
        )
        .await;
    assert_eq!(
        (status, &error["code"]),
        (StatusCode::NOT_FOUND, &"not_found".into())
    );
    // Only a root ID names a destination, never a path.
    let (status, _) = client
        .put(
            "/v1/settings",
            &choose(&updated["revision"], &"/etc".into()),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let data = body(MIB);
    server.put("/to-media.bin", Resource::new(data.clone()));
    let id = client.submit(&server.url("/to-media.bin")).await;
    let done = client.stage(&id, "complete").await;
    assert_eq!(done["destination"], media_id);
    assert_eq!(std::fs::read(media.join("to-media.bin")).unwrap(), data);
    daemon.stop("TERM").await;

    let daemon = Daemon::start(&dirs).await;
    let (_, settings) = daemon.owner().get("/v1/settings").await;
    assert_eq!(settings["default_root"], media_id);
    assert_eq!(settings["revision"], updated["revision"]);
    daemon.stop("TERM").await;
}

/// `GET /v1/folders` for `path`, with the path encoded as a query.
fn folders_at(path: &std::path::Path) -> String {
    let mut url = reqwest::Url::parse("http://local/v1/folders").unwrap();
    url.query_pairs_mut()
        .append_pair("path", path.to_str().unwrap());
    format!("{}?{}", url.path(), url.query().unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_owner_adds_a_server_folder_by_browsing_to_it() {
    let server = Server::https().await;
    let dirs = Dirs::ready(&server);
    let daemon = Daemon::start(&dirs).await;
    let client = daemon.owner();
    let parent = dirs.tmp.path().join("shares");
    std::fs::create_dir_all(parent.join("films")).unwrap();
    std::fs::create_dir_all(parent.join(".cache")).unwrap();

    // Browsing and adding need `control`.
    let reader = daemon.client(READER_TOKEN);
    assert_eq!(
        reader.get(&folders_at(&parent)).await.0,
        StatusCode::FORBIDDEN
    );
    let add = json!({ "path": parent.join("films") });
    assert_eq!(
        reader.post("/v1/roots", &add).await.0,
        StatusCode::FORBIDDEN
    );

    let (status, listing) = client.get(&folders_at(&parent)).await;
    assert_eq!(status, StatusCode::OK, "{listing}");
    let real = std::fs::canonicalize(&parent).unwrap();
    assert_eq!(listing["path"], real.to_str().unwrap());
    assert_eq!(listing["folders"], json!(["films"]));
    assert_eq!(listing["truncated"], false);
    let (status, error) = client.get(&folders_at(&parent.join("missing"))).await;
    assert_eq!(
        (status, &error["code"]),
        (StatusCode::NOT_FOUND, &"not_found".into())
    );
    let (status, _) = client.get("/v1/folders?path=relative").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, root) = client.post("/v1/roots", &add).await;
    assert_eq!(status, StatusCode::OK, "{root}");
    assert_eq!(root["available"], true);
    let (_, settings) = client.get("/v1/settings").await;
    assert!(
        settings["roots"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == root["id"]),
        "{settings}"
    );
    assert_eq!(root["added"], true);

    // A folder the user added can be removed, with `control`; the
    // operator's cannot. Its files stay.
    std::fs::write(parent.join("films/kept.bin"), b"kept").unwrap();
    let removal = |root: &serde_json::Value| {
        client.command(json!({ "expected_revision": settings["revision"], "root": root["id"] }))
    };
    let remove = removal(&root);
    assert_eq!(
        reader.post("/v1/roots/remove", &remove).await.0,
        StatusCode::FORBIDDEN
    );
    let configured = settings["roots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["added"] == false)
        .unwrap();
    let (status, error) = client.post("/v1/roots/remove", &removal(configured)).await;
    assert_eq!(
        (status, &error["code"]),
        (StatusCode::CONFLICT, &"not_applicable".into())
    );
    let (status, removed) = client.post("/v1/roots/remove", &remove).await;
    assert_eq!(status, StatusCode::OK, "{removed}");
    assert_eq!(removed["type"], "settings_updated");
    let (_, settings) = client.get("/v1/settings").await;
    assert!(
        !settings["roots"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == root["id"]),
        "{settings}"
    );
    assert!(parent.join("films/kept.bin").exists());
    // The daemon's own state cannot become a destination.
    let (status, error) = client
        .post("/v1/roots", &json!({ "path": dirs.state }))
        .await;
    assert_eq!(
        (status, &error["code"]),
        (StatusCode::BAD_REQUEST, &"invalid_request".into())
    );
    daemon.stop("TERM").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn finished_jobs_are_renamed_and_removed_over_http() {
    let server = Server::https().await;
    let dirs = Dirs::ready(&server);
    let daemon = Daemon::start(&dirs).await;
    let client = daemon.owner();
    let data = body(1024);
    server.put("/a.bin", Resource::new(data.clone()));
    let job = client.submit(&server.url("/a.bin")).await;
    let done = client.stage(&job, "complete").await;
    let path = format!("/v1/jobs/{job}/commands");

    // Renaming needs a name, and only a rename takes one.
    let rename = |name: &str| {
        client.command(json!({
            "action": "rename",
            "expected_revision": done["revision"],
            "name": name,
        }))
    };
    let nameless =
        client.command(json!({"action": "rename", "expected_revision": done["revision"]}));
    assert_eq!(
        client.post(&path, &nameless).await.0,
        StatusCode::BAD_REQUEST
    );
    std::fs::write(dirs.root.join("taken.bin"), b"user data").unwrap();
    let (status, error) = client.post(&path, &rename("taken.bin")).await;
    assert_eq!(
        (status, &error["code"]),
        (StatusCode::CONFLICT, &"already_exists".into())
    );
    let (status, error) = client.post(&path, &rename("../b.bin")).await;
    assert_eq!(
        (status, &error["code"]),
        (StatusCode::BAD_REQUEST, &"invalid_request".into())
    );
    let (status, renamed) = client.post(&path, &rename("b.bin")).await;
    assert_eq!(status, StatusCode::OK, "{renamed}");
    assert_eq!(renamed["changed"], true);
    assert_eq!(dirs.published("b.bin"), data);
    assert!(!dirs.root.join("a.bin").exists());
    let renamed_job = client.job(&job).await;
    assert_eq!(renamed_job["artifact"]["path"], "b.bin");

    // The link needs the sources permission.
    let (status, source) = client.get(&format!("/v1/jobs/{job}/source")).await;
    assert_eq!(status, StatusCode::OK, "{source}");
    assert_eq!(source["url"], server.url("/a.bin"));
    let (status, _) = daemon
        .client(READER_TOKEN)
        .get(&format!("/v1/jobs/{job}/source"))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Removing hides the job and keeps its file.
    let mut events = client.events(None).await;
    events.until(|e, _| e.event == "hello").await;
    let remove = client.command(json!({
        "action": "remove",
        "expected_revision": renamed_job["revision"],
    }));
    let (status, removed) = client.post(&path, &remove).await;
    assert_eq!(status, StatusCode::OK, "{removed}");
    events
        .until(|e, data| {
            e.event == "event" && data["kind"] == json!({"type": "job_removed", "job": job})
        })
        .await;
    assert!(client.jobs().await.is_empty());
    let (_, counts) = client.get("/v1/job-counts").await;
    assert_eq!(counts["stages"], json!({}));
    let (status, _) = client.get(&format!("/v1/jobs/{job}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(dirs.published("b.bin"), data);

    daemon.stop("TERM").await;
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
    assert_eq!(scopes, "read,submit,control,sources");
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

#[tokio::test(flavor = "multi_thread")]
async fn clients_that_stop_sending_cannot_hold_connections() {
    let server = Server::https().await;
    let dirs = Dirs::ready(&server);
    let daemon = Daemon::start(&dirs).await;
    let address = daemon.base.trim_start_matches("http://").to_owned();

    // Take every connection with a read-only request whose body stalls
    // after its headers.
    let stalled_post = format!(
        "POST /v1/inputs HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {READER_TOKEN}\r\n\
         Content-Type: application/json\r\nContent-Length: 100\r\n\r\n{{"
    );
    let mut held = Vec::new();
    for _ in 0..MAX_CONNECTIONS {
        let mut tcp = tokio::net::TcpStream::connect(&address).await.unwrap();
        tcp.write_all(stalled_post.as_bytes()).await.unwrap();
        held.push(tcp);
    }
    let owner = daemon.owner();
    let blocked = tokio::time::timeout(Duration::from_secs(2), owner.get("/v1/capabilities"));
    assert!(blocked.await.is_err(), "every connection should be taken");

    // The body deadline frees them, and the owner gets through.
    let (status, _) = tokio::time::timeout(
        READ_DEADLINE + Duration::from_secs(10),
        owner.get("/v1/capabilities"),
    )
    .await
    .expect("served once the stalled connections expire");
    assert_eq!(status, StatusCode::OK);
    for tcp in &mut held {
        let mut response = Vec::new();
        tokio::time::timeout(DEADLINE, tcp.read_to_end(&mut response))
            .await
            .expect("the daemon closes the connection")
            .unwrap();
        let response = String::from_utf8_lossy(&response);
        assert!(response.starts_with("HTTP/1.1 408"), "{response}");
    }

    // A connection that never sends anything is closed by the header
    // deadline.
    let mut idle = tokio::net::TcpStream::connect(&address).await.unwrap();
    let mut nothing = Vec::new();
    tokio::time::timeout(
        READ_DEADLINE + Duration::from_secs(10),
        idle.read_to_end(&mut nothing),
    )
    .await
    .expect("the daemon closes an idle connection")
    .unwrap();
    assert!(owner.jobs().await.is_empty());
    daemon.stop("TERM").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn readiness_needs_no_token_and_requires_a_usable_root() {
    let server = Server::https().await;
    let dirs = Dirs::new(&server);
    let probe = |address: &str| {
        std::process::Command::new(BIN)
            .args(["ready", "--address", address.trim_start_matches("http://")])
            .output()
            .unwrap()
    };

    // Serving, but the root was never initialized: not ready, and the
    // reason names no path.
    let daemon = Daemon::start(&dirs).await;
    let anonymous = Client::new(&daemon.base, &daemon.instance, None);
    let (status, ready) = anonymous.get("/v1/ready").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        ready,
        json!({"ready": false, "reason": "download_root_unavailable"})
    );
    let out = probe(&daemon.base);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not ready: download_root_unavailable"),
        "{stderr}"
    );
    // Readiness is the only route without a token.
    let (status, _) = anonymous.get("/v1/capabilities").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    daemon.stop("TERM").await;

    // The probe starts no engine of its own: it fails while nothing serves.
    let out = probe("127.0.0.1:9");
    assert!(!out.status.success());

    dirs.init_root();
    let daemon = Daemon::start(&dirs).await;
    let anonymous = Client::new(&daemon.base, &daemon.instance, None);
    let (status, ready) = anonymous.get("/v1/ready").await;
    assert_eq!((status, ready), (StatusCode::OK, json!({"ready": true})));
    let out = probe(&daemon.base);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ready\n");

    // A root that disappears while serving makes the daemon not ready.
    let moved = dirs.tmp.path().join("moved");
    std::fs::rename(&dirs.root, &moved).unwrap();
    std::fs::create_dir(&dirs.root).unwrap();
    let (status, _) = anonymous.get("/v1/ready").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    std::fs::remove_dir(&dirs.root).unwrap();
    std::fs::rename(&moved, &dirs.root).unwrap();
    let (status, _) = anonymous.get("/v1/ready").await;
    assert_eq!(status, StatusCode::OK);
    daemon.stop("TERM").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stuck_readiness_check_neither_piles_up_nor_holds_up_exit() {
    let server = Server::https().await;
    let dirs = Dirs::ready(&server);
    let daemon = Daemon::start(&dirs).await;
    let anonymous = Client::new(&daemon.base, &daemon.instance, None);
    let (status, _) = anonymous.get("/v1/ready").await;
    assert_eq!(status, StatusCode::OK);

    // The root's sentinel becomes a FIFO nobody writes to: opening it
    // blocks, as a read on a hung mount would.
    let sentinel = dirs.root.join(".wayfetch").join("root.json");
    std::fs::remove_file(&sentinel).unwrap();
    let made = std::process::Command::new("mkfifo")
        .arg(&sentinel)
        .status()
        .unwrap();
    assert!(made.success());

    // Every request is answered within the readiness deadline.
    for _ in 0..3 {
        let started = std::time::Instant::now();
        let (status, ready) = anonymous.get("/v1/ready").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(ready, json!({"ready": false, "reason": "check_timed_out"}));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }

    // The check is still stuck, yet SIGTERM ends the process promptly.
    let started = std::time::Instant::now();
    daemon.stop("TERM").await;
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
}

/// A browser of one daemon: sends its origin, its session cookie, and its
/// CSRF token, as a same-origin page would.
struct Browser {
    client: Client,
    origin: String,
    cookie: Option<String>,
    csrf: Option<String>,
}

impl Browser {
    fn new(daemon: &Daemon) -> Self {
        Self {
            client: Client::new(&daemon.base, &daemon.instance, None),
            origin: daemon.base.clone(),
            cookie: None,
            csrf: None,
        }
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let mut request = self
            .client
            .request(method, path)
            .header("origin", &self.origin)
            .header("sec-fetch-site", "same-origin");
        if let Some(cookie) = &self.cookie {
            request = request.header("cookie", cookie);
        }
        if let Some(csrf) = &self.csrf {
            request = request.header("x-csrf-token", csrf);
        }
        request
    }

    async fn get(&self, path: &str) -> (StatusCode, Value) {
        self.client
            .send(self.request(reqwest::Method::GET, path))
            .await
    }

    async fn post(&self, path: &str, body: &Value) -> (StatusCode, Value) {
        self.client
            .send(self.request(reqwest::Method::POST, path).json(body))
            .await
    }

    /// Posts sign-in or setup `body` to `path`; on success keeps the
    /// session cookie and CSRF token.
    async fn sign_in(&mut self, path: &str, body: &Value) -> (StatusCode, Value) {
        let response = self
            .request(reqwest::Method::POST, path)
            .json(body)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let set_cookie = response
            .headers()
            .get("set-cookie")
            .map(|v| v.to_str().unwrap().to_owned());
        assert_eq!(response.headers()["cache-control"], "no-store");
        let result: Value = response.json().await.unwrap();
        if status == StatusCode::OK {
            let set_cookie = set_cookie.expect("a session cookie");
            for attribute in ["HttpOnly", "SameSite=Strict", "Path=/"] {
                assert!(set_cookie.contains(attribute), "{set_cookie}");
            }
            self.cookie = Some(set_cookie.split(';').next().unwrap().to_owned());
            self.csrf = Some(result["csrf_token"].as_str().unwrap().to_owned());
        }
        (status, result)
    }

    /// The event stream, opened with the session.
    async fn events(&self) -> Events {
        let response = self
            .request(reqwest::Method::GET, "/v1/events")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        Events(Box::pin(response.bytes_stream().eventsource()))
    }
}

const PASSWORD: &str = "correct horse battery staple";

#[tokio::test(flavor = "multi_thread")]
async fn browsers_set_up_the_owner_sign_in_and_use_the_api() {
    let server = Server::https().await;
    let mut dirs = Dirs::ready(&server);
    dirs.browser = true;
    server.put("/a.bin", Resource::new(body(1024)));
    let daemon = Daemon::start(&dirs).await;
    let issued = daemon
        .transcript
        .iter()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .find(|v| v["type"] == "setup_secret_issued")
        .expect("a setup secret");
    assert_eq!(issued["valid_for_secs"], 15 * 60);
    let secret_path = PathBuf::from(issued["path"].as_str().unwrap());
    let secret = std::fs::read_to_string(&secret_path).unwrap();
    let secret = secret.trim();

    let mut browser = Browser::new(&daemon);
    let (status, session) = browser.get("/v1/auth/session").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        session,
        json!({"setup_required": true, "authenticated": false})
    );
    // Nothing else answers without credentials.
    assert_eq!(browser.get("/v1/jobs").await.0, StatusCode::UNAUTHORIZED);

    // Setup needs the right origin and secret; it works once.
    let setup = json!({"setup_secret": secret, "username": "owner", "password": PASSWORD});
    let mut evil = Browser::new(&daemon);
    evil.origin = "https://evil.test".to_owned();
    assert_eq!(
        evil.sign_in("/v1/auth/setup", &setup).await.0,
        StatusCode::FORBIDDEN
    );
    let wrong = json!({"setup_secret": "0".repeat(64), "username": "owner", "password": PASSWORD});
    assert_eq!(
        browser.sign_in("/v1/auth/setup", &wrong).await.0,
        StatusCode::UNAUTHORIZED
    );
    let (status, signed_in) = browser.sign_in("/v1/auth/setup", &setup).await;
    assert_eq!(status, StatusCode::OK, "{signed_in}");
    assert_eq!(signed_in["username"], "owner");
    assert!(!secret_path.exists());

    // The session reads, submits with its CSRF token, and streams events.
    let (_, session) = browser.get("/v1/auth/session").await;
    assert_eq!(session["authenticated"], true);
    assert_eq!(session["csrf_token"], signed_in["csrf_token"]);
    assert_eq!(
        session["permissions"],
        json!(["read", "submit", "control", "sources"])
    );
    let mut events = browser.events().await;
    events.until(|e, _| e.event == "hello").await;
    let submit = browser
        .client
        .command(json!({"items": [server.url("/a.bin")]}));
    let mut forged = Browser {
        csrf: None,
        ..Browser::new(&daemon)
    };
    forged.cookie.clone_from(&browser.cookie);
    assert_eq!(
        forged.post("/v1/inputs", &submit).await.0,
        StatusCode::FORBIDDEN
    );
    forged.csrf.clone_from(&browser.csrf);
    forged.origin = "https://evil.test".to_owned();
    assert_eq!(
        forged.post("/v1/inputs", &submit).await.0,
        StatusCode::FORBIDDEN
    );
    // DNS rebinding: the right origin, but another name for the server.
    let rebound = browser
        .request(reqwest::Method::GET, "/v1/jobs")
        .header("host", "evil.test");
    assert_eq!(browser.client.send(rebound).await.0, StatusCode::FORBIDDEN);
    let (status, result) = browser.post("/v1/inputs", &submit).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let (status, page) = browser.get("/v1/jobs").await;
    assert_eq!(status, StatusCode::OK);
    let job = page["items"][0]["id"].as_str().unwrap().to_owned();
    events.stage(&job, "complete").await;

    // Bearer tokens work as before, cookies or not.
    let owner = daemon.owner();
    assert_eq!(owner.job(&job).await["stage"], "complete");
    let reader = browser
        .request(reqwest::Method::POST, "/v1/inputs")
        .bearer_auth(READER_TOKEN)
        .json(&submit);
    assert_eq!(browser.client.send(reader).await.0, StatusCode::FORBIDDEN);

    // Sign-out ends the session and the streams opened with it, before
    // anything submitted afterwards reaches them; failures say only that
    // sign-in failed.
    let (status, _) = browser.post("/v1/auth/logout", &json!({})).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(browser.get("/v1/jobs").await.0, StatusCode::UNAUTHORIZED);
    server.put("/b.bin", Resource::new(body(1024)));
    let after = owner.submit(&server.url("/b.bin")).await;
    owner.stage(&after, "complete").await;
    while let Some(event) = events.next().await {
        assert!(!event.data.contains(&after), "{}", event.data);
    }
    let mut other = Browser::new(&daemon);
    let (status, wrong_password) = other
        .sign_in(
            "/v1/auth/login",
            &json!({"username": "owner", "password": "not the password"}),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (_, wrong_name) = other
        .sign_in(
            "/v1/auth/login",
            &json!({"username": "nobody", "password": PASSWORD}),
        )
        .await;
    assert_eq!(wrong_password, wrong_name);
    let login = json!({"username": "owner", "password": PASSWORD});
    assert_eq!(
        other.sign_in("/v1/auth/login", &login).await.0,
        StatusCode::OK
    );
    assert_eq!(other.get("/v1/jobs").await.0, StatusCode::OK);
    // Five attempts a minute, setup included; even the right password
    // waits after that.
    let limited = Browser::new(&daemon)
        .request(reqwest::Method::POST, "/v1/auth/login")
        .json(&login)
        .send()
        .await
        .unwrap();
    assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(limited.headers().contains_key("retry-after"));

    // A restart keeps the owner but ends every session.
    daemon.stop("TERM").await;
    let mut daemon = Daemon::start(&dirs).await;
    assert!(
        !daemon
            .transcript
            .iter()
            .any(|l| l.contains("setup_secret_issued"))
    );
    other.client = Client::new(&daemon.base, &daemon.instance, None);
    other.origin.clone_from(&daemon.base);
    assert_eq!(other.get("/v1/jobs").await.0, StatusCode::UNAUTHORIZED);
    let mut again = Browser::new(&daemon);
    assert_eq!(
        again.sign_in("/v1/auth/login", &login).await.0,
        StatusCode::OK
    );

    // Recovery: reset the owner while stopped, and setup is offered again.
    let reset = || {
        std::process::Command::new(BIN)
            .arg("reset-owner")
            .arg("--auth-dir")
            .arg(dirs.auth())
            .output()
            .unwrap()
    };
    let refused = reset();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("in use"));
    daemon.signal("TERM");
    let finished = daemon.finish().await;
    assert!(finished.status.success(), "{}", finished.stderr);
    let everything = format!("{:?}{}", finished.lines, finished.stderr);
    assert!(!everything.contains(secret) && !everything.contains(PASSWORD));
    assert!(reset().status.success());
    daemon = Daemon::start(&dirs).await;
    let (_, session) = Browser::new(&daemon).get("/v1/auth/session").await;
    assert_eq!(session["setup_required"], true);
    daemon.stop("TERM").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn insecure_browser_access_is_refused_beyond_loopback() {
    let server = Server::https().await;
    let dirs = Dirs::ready(&server);
    for (listen, args, expected) in [
        (
            "0.0.0.0:0",
            &["--dev-insecure-loopback"][..],
            "needs a loopback listener",
        ),
        (
            "127.0.0.1:0",
            &["--browser-origin", "http://192.168.1.2:7878"][..],
            "use HTTPS",
        ),
        ("127.0.0.1:0", &[][..], "at least one browser origin"),
    ] {
        let out = Daemon::command_on(&dirs, listen)
            .arg("--auth-dir")
            .arg(dirs.auth())
            .args(args)
            .output()
            .await
            .unwrap();
        assert!(!out.status.success(), "{args:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(expected), "{args:?}: {stderr}");
    }
    // Refused before anything was written.
    assert!(!dirs.auth().exists());
}
