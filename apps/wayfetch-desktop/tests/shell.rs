//! The real desktop shell against a local fixture server (architecture
//! brief sections 11.6 and 11.7, gate G1a): the built `wayfetch-desktop`
//! opens its window in the system WebView, driven through WebKitWebDriver
//! as a person would, and closed with `WM_DELETE_WINDOW` as a window
//! manager does.
//!
//! Covered: a download submitted and observed through the shared UI;
//! pause, resume, and cancel over IPC; a renderer reload mid-transfer;
//! graceful close, relaunch on the same state directory, recovery, and the
//! final bytes; a second engine refused; stale revisions, wrong-instance
//! commands, idempotency, resynchronization, a stalled renderer's
//! subscription ending, and the per-renderer subscription limit and its
//! cleanup on reload; that the page loads only bundled assets; and that the
//! host starts no daemon and listens on no TCP port.
//!
//! These tests need an X display (`DISPLAY`, for example from `xvfb-run`)
//! and `WebKitWebDriver` on `PATH`, so they are ignored by default; see
//! the README's desktop section.

#![cfg(target_os = "linux")]

#[path = "../../../crates/engine/tests/support/mod.rs"]
mod engine_support;
#[path = "../../../crates/net/tests/support/mod.rs"]
mod support;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use engine_support::body;
use fantoccini::key::Key;
use fantoccini::{Client, ClientBuilder, Locator};
use serde_json::{Value, json};
use support::{Resource, Server};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};

const BIN: &str = env!("CARGO_BIN_EXE_wayfetch-desktop");
/// How long any one wait may take.
const DEADLINE: Duration = Duration::from_secs(60);
const MIB: usize = 1024 * 1024;
/// Past two of the engine's default 8 MiB checkpoints, so there are
/// durable bytes to resume from while the transfer stalls.
const SIZE: usize = 20 * MIB + 11;
const STALL: usize = 17 * MIB + 5;
/// The host's limits (`src/subscriptions.rs`).
const MAX_PER_RENDERER: usize = 4;
const WINDOW: usize = 64;

/// A state directory and an initialized download root.
struct Dirs {
    _tmp: tempfile::TempDir,
    state: PathBuf,
    root: PathBuf,
}

impl Dirs {
    async fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let (state, root) = (tmp.path().join("state"), tmp.path().join("downloads"));
        std::fs::create_dir(&root).unwrap();
        let init = Command::new(BIN)
            .arg("init-root")
            .arg("--state-dir")
            .arg(&state)
            .arg(&root)
            .output()
            .await
            .unwrap();
        assert!(init.status.success(), "{init:?}");
        Self {
            _tmp: tmp,
            state,
            root,
        }
    }

    fn run_args(&self) -> Vec<String> {
        [
            "run",
            "--json",
            "--state-dir",
            &self.state.display().to_string(),
            "--download-root",
            &self.root.display().to_string(),
            // Only the fixture's loopback address is reachable.
            "--allow-network",
            "127.0.0.1/32",
        ]
        .map(String::from)
        .to_vec()
    }

    fn published(&self, name: &str) -> Vec<u8> {
        std::fs::read(self.root.join(name)).unwrap()
    }
}

/// The JSON notices a process printed.
#[derive(Clone, Default)]
struct Notices(Arc<Mutex<Vec<Value>>>);

impl Notices {
    fn collect(&self, from: impl AsyncRead + Unpin + Send + 'static) {
        let notices = self.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(from).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Ok(notice @ Value::Object(_)) = serde_json::from_str(&line) {
                    notices.0.lock().unwrap().push(notice);
                }
            }
        });
    }

    fn all(&self) -> Vec<Value> {
        self.0.lock().unwrap().clone()
    }

    fn of_type(&self, kind: &str) -> Vec<Value> {
        self.all()
            .into_iter()
            .filter(|n| n["type"] == kind)
            .collect()
    }

    /// Waits for a notice `matches` accepts, among those after `skip`.
    async fn wait(&self, skip: usize, matches: impl Fn(&Value) -> bool) -> Value {
        eventually(|| async { self.all().into_iter().skip(skip).find(&matches) }).await
    }
}

/// Polls `check` until it returns something, or panics at the deadline.
async fn eventually<T, F: Future<Output = Option<T>>>(mut check: impl FnMut() -> F) -> T {
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        if let Some(value) = check().await {
            return value;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// The desktop app, launched by WebKitWebDriver with automation enabled.
struct Shell {
    driver: Child,
    client: Client,
    /// The `wayfetch-desktop` process.
    app: u32,
    /// What the app printed (it inherits the driver's output).
    notices: Notices,
}

impl Shell {
    async fn launch(dirs: &Dirs) -> Self {
        let port = free_port();
        let mut driver = Command::new("WebKitWebDriver")
            .arg(format!("--port={port}"))
            // Lets Tauri's WebView accept the driver's automation session.
            .env("TAURI_WEBVIEW_AUTOMATION", "true")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("WebKitWebDriver is installed");
        let notices = Notices::default();
        notices.collect(driver.stdout.take().unwrap());
        let url = format!("http://127.0.0.1:{port}");
        eventually(|| async {
            tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .ok()
        })
        .await;
        let mut capabilities = serde_json::Map::new();
        capabilities.insert(
            "webkitgtk:browserOptions".into(),
            json!({"binary": BIN, "args": dirs.run_args()}),
        );
        let client = ClientBuilder::new(hyper_util::client::legacy::connect::HttpConnector::new())
            .capabilities(capabilities)
            .connect(&url)
            .await
            .expect("the window opens under WebKitWebDriver");
        let app = children(driver.id().unwrap())
            .into_iter()
            .find(|pid| exe(*pid).as_deref() == Some(Path::new(BIN)))
            .expect("the driver launched the app");
        let shell = Self {
            driver,
            client,
            app,
            notices,
        };
        shell
            .wait_for("document.querySelector('.statusbar .status')?.textContent === 'Live'")
            .await;
        shell
    }

    /// Runs `script` in the page and returns its value.
    async fn js(&self, script: &str) -> Value {
        self.client.execute(script, vec![]).await.unwrap()
    }

    /// Runs an async function body in the page and returns its value. The
    /// body can use `client`, a `DesktopIpcClient`, and `core`, Tauri's
    /// IPC.
    async fn ipc(&self, body: &str) -> Value {
        let script = format!(
            "const done = arguments[arguments.length - 1];
             (async () => {{
               const {{ DesktopIpcClient }} = await import('/transports/ipc.js');
               const core = window.__TAURI__.core;
               const client = new DesktopIpcClient(core);
               {body}
             }})().then(done, (e) => done({{ thrown: e?.code ?? String(e) }}));"
        );
        self.client.execute_async(&script, vec![]).await.unwrap()
    }

    /// Waits until `expression` is truthy in the page.
    async fn wait_for(&self, expression: &str) {
        eventually(|| async {
            let value = self
                .client
                .execute(&format!("return Boolean({expression})"), vec![])
                .await
                .ok()?;
            (value == json!(true)).then_some(())
        })
        .await;
    }

    async fn wait_for_state(&self, job: &str, state: &str) {
        self.wait_for(&format!(
            "document.querySelector('tr[data-job=\"{job}\"] td[data-state]')?.textContent === '{state}'"
        ))
        .await;
    }

    async fn click(&self, css: &str) {
        self.client
            .wait()
            .at_most(DEADLINE)
            .for_element(Locator::Css(css))
            .await
            .unwrap()
            .click()
            .await
            .unwrap();
    }

    /// Adds links, one per line, through the toolbar's form, which then
    /// says `added`.
    async fn add_many(&self, urls: &str, added: &str) {
        let open = self
            .js("return document.querySelector('textarea[name=\"urls\"]')?.checkVisibility() ?? false")
            .await;
        if open != json!(true) {
            self.click("button[aria-controls=\"add-links\"]").await;
        }
        let links = self
            .client
            .find(Locator::Css("textarea[name=\"urls\"]"))
            .await
            .unwrap();
        links.clear().await.unwrap();
        // One line at a time: WebKitWebDriver drops a newline inside typed
        // text.
        for (i, url) in urls.lines().enumerate() {
            if i > 0 {
                links
                    .send_keys(&char::from(Key::Enter).to_string())
                    .await
                    .unwrap();
            }
            links.send_keys(url).await.unwrap();
        }
        self.click("#add-links button[type=\"submit\"]").await;
        self.wait_for(&format!(
            "document.querySelector('#add-links .message')?.textContent === '{added}'"
        ))
        .await;
    }

    /// Adds a link through the toolbar's form.
    async fn add(&self, url: &str) {
        self.add_many(url, "Added 1 download.").await;
    }

    /// Selects a job, so the toolbar acts on it.
    async fn select(&self, job: &str) {
        self.click(&format!("tr[data-job=\"{job}\"] td.name a"))
            .await;
        self.wait_for(&format!(
            "document.querySelector('button[data-action=\"pause\"]')?.getAttribute('aria-label')?.endsWith('{job}')"
        ))
        .await;
    }

    async fn act(&self, action: &str, job: &str) {
        self.click(&format!(
            "button[data-action=\"{action}\"][aria-label$=\"{job}\"]:not([disabled])"
        ))
        .await;
    }

    /// Reloads the page, as a person pressing F5 would.
    async fn reload(&self) {
        self.client.refresh().await.unwrap();
        self.wait_for("document.querySelector('.statusbar .status')?.textContent === 'Live'")
            .await;
    }

    /// Closes the window the way a window manager does, waits for the app
    /// to exit, and returns what it printed.
    async fn close(mut self) -> Vec<Value> {
        close_window(self.app);
        let app = self.app;
        eventually(|| async { (!running(app)).then_some(()) }).await;
        // The session's browser is gone; the driver only needs to stop.
        let _ = self.client.close().await;
        let _ = self.driver.kill().await;
        self.notices.all()
    }
}

/// Child processes of `pid`.
fn children(pid: u32) -> Vec<u32> {
    std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
        .unwrap_or_default()
        .split_whitespace()
        .map(|p| p.parse().unwrap())
        .collect()
}

/// `pid` and all its descendants.
fn tree(pid: u32) -> Vec<u32> {
    let mut all = vec![pid];
    let mut i = 0;
    while i < all.len() {
        all.extend(children(all[i]));
        i += 1;
    }
    all
}

fn exe(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

/// Whether `pid` is running (not gone or a zombie).
fn running(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        !stat
            .rsplit(')')
            .next()
            .unwrap_or("")
            .trim_start()
            .starts_with('Z')
    })
}

/// Socket inodes `pid` holds open.
fn socket_inodes(pid: u32) -> HashSet<String> {
    let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return HashSet::new();
    };
    fds.filter_map(|fd| std::fs::read_link(fd.ok()?.path()).ok())
        .filter_map(|target| {
            let target = target.to_str()?;
            Some(
                target
                    .strip_prefix("socket:[")?
                    .strip_suffix(']')?
                    .to_owned(),
            )
        })
        .collect()
}

/// The TCP ports that `pids` listen on.
fn listening_ports(pids: &[u32]) -> Vec<u16> {
    let held: HashSet<_> = pids.iter().flat_map(|p| socket_inodes(*p)).collect();
    ["/proc/net/tcp", "/proc/net/tcp6"]
        .iter()
        .flat_map(|table| {
            let table = std::fs::read_to_string(table).unwrap_or_default();
            table.lines().skip(1).map(str::to_owned).collect::<Vec<_>>()
        })
        .filter_map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            // State 0A is LISTEN; the inode is the tenth field.
            if fields.get(3) != Some(&"0A") || !held.contains(*fields.get(9)?) {
                return None;
            }
            let (_, port) = fields.get(1)?.rsplit_once(':')?;
            u16::from_str_radix(port, 16).ok()
        })
        .collect()
}

/// The port of the WebKit inspector server WebKitWebDriver asked `pid`
/// to open for its automation session.
fn automation_port(pid: u32) -> Option<u16> {
    let environ = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    environ
        .split(|b| *b == 0)
        .filter_map(|var| std::str::from_utf8(var).ok())
        .find_map(|var| var.strip_prefix("WEBKIT_INSPECTOR_SERVER="))
        .and_then(|address| address.rsplit_once(':')?.1.parse().ok())
}

/// Checks that `app` runs the engine itself: its only descendants are the
/// WebView's processes (no daemon, nothing else), and the TCP ports its
/// tree listens on are exactly `allowed`.
fn assert_embedded(app: u32, allowed: &[u16]) {
    let processes = tree(app);
    for pid in &processes[1..] {
        let name = exe(*pid).unwrap();
        let name = name.file_name().unwrap().to_str().unwrap();
        assert!(
            name == "WebKitWebProcess" || name == "WebKitNetworkProcess",
            "unexpected child process {name}"
        );
    }
    assert_eq!(listening_ports(&processes), allowed);
}

/// Sends `WM_DELETE_WINDOW` to the top-level windows of `pid`, as a
/// window manager does when the close button is pressed.
fn close_window(pid: u32) {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{AtomEnum, ClientMessageEvent, ConnectionExt, EventMask, Window};

    let (conn, screen) = x11rb::connect(None).expect("an X display");
    let atom = |name: &str| {
        conn.intern_atom(false, name.as_bytes())
            .unwrap()
            .reply()
            .unwrap()
            .atom
    };
    let (protocols, delete, wm_pid) = (
        atom("WM_PROTOCOLS"),
        atom("WM_DELETE_WINDOW"),
        atom("_NET_WM_PID"),
    );
    let root = conn.setup().roots[screen].root;
    let windows: Vec<Window> = conn.query_tree(root).unwrap().reply().unwrap().children;
    let mut sent = 0;
    for window in windows {
        let owner = conn
            .get_property(false, window, wm_pid, AtomEnum::CARDINAL, 0, 1)
            .unwrap()
            .reply()
            .ok()
            .and_then(|p| p.value32()?.next());
        if owner != Some(pid) {
            continue;
        }
        let event = ClientMessageEvent::new(32, window, protocols, [delete, 0, 0, 0, 0]);
        conn.send_event(false, window, EventMask::NO_EVENT, event)
            .unwrap();
        sent += 1;
    }
    conn.flush().unwrap();
    assert!(sent > 0, "no window of process {pid}");
}

/// A fixture serving `name` with `SIZE` bytes that stalls at `STALL`.
fn stalled(server: &Server, name: &str, size: usize, stall: usize) -> String {
    let mut resource = Resource::new(body(size));
    resource.stall_at = Some(stall);
    server.put(&format!("/{name}"), resource);
    server.url(&format!("/{name}"))
}

fn release(server: &Server, name: &str) {
    server.update(&format!("/{name}"), |r| r.stall_at = None);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs an X display and WebKitWebDriver"]
async fn downloads_through_the_shared_ui_and_recovers_after_relaunch() {
    let server = Server::http().await;
    let dirs = Dirs::new().await;
    let shell = Shell::launch(&dirs).await;
    shell
        .wait_for("document.body.textContent.includes('No downloads yet.')")
        .await;

    // Everything the page loaded is bundled in the app.
    let loaded = shell
        .js(
            "return [...document.querySelectorAll('script[src], link[href]')]
                .map((e) => e.src || e.href)
                .concat([...document.styleSheets].map((s) => s.href).filter(Boolean))",
        )
        .await;
    for url in loaded.as_array().unwrap() {
        assert!(
            url.as_str().unwrap().starts_with("tauri://localhost/"),
            "{url}"
        );
    }
    // The page's policy refuses to parse strings as HTML (Trusted Types),
    // and the renderer has no Tauri API beyond the host's commands.
    let refused = shell
        .js(
            "try { document.createElement('div').innerHTML = '<b>x</b>'; return 'parsed'; }
             catch (e) { return e.name; }",
        )
        .await;
    assert_eq!(refused, "TypeError");
    let refusals = shell
        .ipc(
            "const call = (command, args) =>
                core.invoke(command, args).then(() => 'allowed', (e) => String(e));
              return [
                await call('plugin:path|resolve_directory', { directory: 0 }),
                await call('plugin:webview|create_webview_window', {}),
                await call('plugin:fs|read_file', { path: '/etc/passwd' }),
                await call('plugin:shell|execute', { program: 'sh' }),
              ];",
        )
        .await;
    for refusal in refusals.as_array().unwrap() {
        assert!(
            refusal.as_str().unwrap().contains("not allowed"),
            "{refusals}"
        );
    }
    let fonts = shell
        .ipc(
            "await document.fonts.ready;
              return [...document.fonts]
                .filter((f) => f.family.includes('IBM Plex Sans') && f.status === 'loaded')
                .map((f) => f.weight);",
        )
        .await;
    assert!(fonts.as_array().unwrap().contains(&json!("400")), "{fonts}");

    // Submit and observe a download that stalls past two checkpoints.
    let first = stalled(&server, "first.bin", SIZE, STALL);
    shell.add(&first).await;
    shell.wait_for_state("job-1", "Downloading").await;
    shell
        .wait_for("/^1[67]\\.\\d MiB of 20\\.0 MiB$/.test(document.querySelector('tr[data-job=\"job-1\"] .bytes')?.textContent)")
        .await;

    // Pause and resume through IPC commands.
    shell.select("job-1").await;
    shell.act("pause", "job-1").await;
    shell.wait_for_state("job-1", "Paused").await;
    shell.act("resume", "job-1").await;
    shell.wait_for_state("job-1", "Downloading").await;

    // A reload closes the page's subscription and the new page opens its
    // own; the job keeps going.
    let before = shell.notices.all().len();
    shell.reload().await;
    shell.wait_for_state("job-1", "Downloading").await;
    shell
        .notices
        .wait(before, |n| {
            n["type"] == "subscriptions" && n["ended"] == "closed"
        })
        .await;
    shell
        .notices
        .wait(before, |n| {
            n["type"] == "subscriptions" && n["open"] == 1 && n.get("ended").is_none()
        })
        .await;
    let open = shell.notices.of_type("subscriptions").last().unwrap()["open"].clone();
    assert_eq!(open, 1, "the old page's subscription was released");

    // Cancel a second download mid-transfer.
    let second = stalled(&server, "second.bin", 10 * MIB, 9 * MIB);
    shell.add(&second).await;
    shell.wait_for_state("job-2", "Downloading").await;
    shell.select("job-2").await;
    shell.act("cancel", "job-2").await;
    shell.wait_for_state("job-2", "Cancelled").await;

    // A long list scrolls inside the table; the page itself never does.
    let many: Vec<_> = (0..50)
        .map(|i| {
            let path = format!("/many-{i}.bin");
            server.put(&path, Resource::new(body(1024)));
            server.url(&path)
        })
        .collect();
    shell
        .add_many(&many.join("\n"), "Added 50 downloads.")
        .await;
    shell
        .wait_for("document.querySelectorAll('tr[data-job]').length === 50")
        .await;
    let sizes = shell
        .js("const table = document.querySelector('.table-scroll');
             return [document.documentElement.scrollHeight, document.documentElement.clientHeight,
                     table.scrollHeight > table.clientHeight];")
        .await;
    assert_eq!(sizes[0], sizes[1], "the page fits the window: {sizes}");
    assert_eq!(sizes[2], true, "the table scrolls: {sizes}");

    // The engine runs in the app's own process. The only TCP listener is
    // the inspector server WebKitWebDriver asked the WebView to open for
    // this automation session; `runs_without_a_daemon_or_listener` checks
    // an undriven launch.
    let automation = automation_port(shell.app).expect("launched by the driver");
    assert_embedded(shell.app, &[automation]);

    // A second engine is refused the state directory, before any window.
    let refused = Command::new(BIN)
        .args(dirs.run_args())
        .output()
        .await
        .unwrap();
    assert!(!refused.status.success());
    let stdout = String::from_utf8_lossy(&refused.stdout);
    assert!(
        stdout.contains("owned by another running engine"),
        "{stdout}"
    );
    assert!(!stdout.contains("\"started\""), "{stdout}");

    // Closing the window stops the engine gracefully and exits.
    let printed = shell.close().await;
    let types: Vec<_> = printed
        .iter()
        .map(|n| n["type"].as_str().unwrap())
        .collect();
    assert!(types.contains(&"shutting_down"), "{printed:?}");
    let stopped = printed.iter().find(|n| n["type"] == "stopped").unwrap();
    assert_eq!(stopped["clean"], true, "{printed:?}");
    assert_eq!(stopped["state_dir_released"], true);
    assert_eq!(
        printed
            .iter()
            .rfind(|n| n["type"] == "subscriptions")
            .unwrap()["open"],
        0
    );

    // Relaunched on the same state directory, the accepted job recovers
    // and finishes once the server sends the rest; the cancelled one stays
    // cancelled.
    let shell = Shell::launch(&dirs).await;
    shell.wait_for_state("job-2", "Cancelled").await;
    shell.wait_for_state("job-1", "Downloading").await;
    release(&server, "first.bin");
    shell.wait_for_state("job-1", "Done").await;
    assert!(
        dirs.published("first.bin") == body(SIZE),
        "final bytes match"
    );
    let resumed = server
        .ranges("/first.bin")
        .into_iter()
        .flatten()
        .any(|(start, _)| start > 0);
    assert!(resumed, "the relaunch resumed from durable bytes");
    let printed = shell.close().await;
    assert!(
        printed
            .iter()
            .any(|n| n["type"] == "stopped" && n["clean"] == true)
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs an X display and WebKitWebDriver"]
async fn ipc_keeps_the_protocol_semantics() {
    let server = Server::http().await;
    let dirs = Dirs::new().await;
    let shell = Shell::launch(&dirs).await;
    let url = stalled(&server, "a.bin", 4 * MIB, 2 * MIB);

    // Idempotency, wrong instance, and stale revisions, as over HTTP.
    let answers = shell
        .ipc(&format!(
            "const hello = await client.hello();
             const target = hello.instance;
             const submit = (key, items, to = target) =>
               client.submit({{ idempotencyKey: key, target: to, items }}).catch((e) => e.code);
             const key = crypto.randomUUID();
             const first = await submit(key, ['{url}']);
             const replay = await submit(key, ['{url}']);
             const reused = await submit(key, ['{url}?other']);
             const wrong = await submit(crypto.randomUUID(), ['{url}'], crypto.randomUUID());
             const invalid = await submit(crypto.randomUUID(), ['{url}', 'ftp://x/']);
             // Revisions move while the job starts; wait until it stalls.
             let job;
             do {{
               await new Promise((resolve) => setTimeout(resolve, 200));
               job = (await client.inspectJob('job-1')).job;
             }} while (job.stage !== 'transferring');
             const command = (action, revision) => client.jobCommand('job-1', {{
               idempotencyKey: crypto.randomUUID(), target, action, expectedRevision: revision,
             }});
             const paused = await command('pause', job.revision);
             let parked;
             do {{
               await new Promise((resolve) => setTimeout(resolve, 200));
               parked = (await client.inspectJob('job-1')).job;
             }} while (parked.stage !== 'paused');
             const stale = await command('resume', job.revision).catch((e) =>
               ({{ code: e.code, current: e.currentRevision, expected: parked.revision }}));
             const resumed = await command('resume', parked.revision);
             const tooBig = await client.listJobs({{ pageSize: 501 }}).catch((e) => e.code);
             const missing = await client.inspectJob('job-99').catch((e) => e.code);
             return {{ first, replay, reused, wrong, invalid, paused, stale, resumed, tooBig, missing }};"
        ))
        .await;
    assert_eq!(answers["first"]["type"], "submitted", "{answers}");
    assert_eq!(answers["replay"], answers["first"]);
    assert_eq!(answers["reused"], "idempotency_conflict");
    assert_eq!(answers["wrong"], "wrong_instance");
    assert_eq!(answers["invalid"], "invalid_request");
    assert_eq!(answers["paused"]["changed"], true);
    assert_eq!(answers["stale"]["code"], "revision_conflict");
    assert_eq!(answers["stale"]["current"], answers["stale"]["expected"]);
    assert_eq!(answers["resumed"]["changed"], true);
    assert_eq!(answers["tooBig"], "limit_exceeded");
    assert_eq!(answers["missing"], "not_found");

    // A cursor the engine cannot honour gets a hello, then a resync, and
    // the stream ends there.
    let streams = shell
        .ipc(
            "const hello = await client.hello();
             const follow = (after) => new Promise((resolve) => {
               const items = [];
               client.subscribe(after, {
                 item: (item) => { items.push(item.type === 'resync' ? item.reason : item.type);
                                   if (item.type === 'resync') setTimeout(() => resolve(items), 300); },
                 end: () => { items.push('end'); resolve(items); },
               });
             });
             return {
               other: await follow({ instance: crypto.randomUUID(), position: 0 }),
               ahead: await follow({ instance: hello.instance, position: hello.head.position + 1000 }),
             };",
        )
        .await;
    assert_eq!(streams["other"], json!(["hello", "unknown_instance"]));
    assert_eq!(streams["ahead"], json!(["hello", "cursor_ahead"]));

    // A renderer holds a bounded number of subscriptions; the page's feed
    // holds one.
    let limit = shell
        .ipc(&format!(
            "const results = [];
             for (let i = 0; i < {MAX_PER_RENDERER}; i += 1) {{
               const channel = new core.Channel();
               results.push(await core.invoke('subscribe', {{ after: null, channel }})
                 .then(() => 'open', (e) => e.code));
             }}
             return results;"
        ))
        .await;
    let mut expected = vec![json!("open"); MAX_PER_RENDERER - 1];
    expected.push(json!("overloaded"));
    assert_eq!(limit, json!(expected));
    // A reload releases them all.
    shell.reload().await;
    let open = shell
        .notices
        .wait(0, |n| {
            n["type"] == "subscriptions" && n["ended"] == "closed" && n["open"] == 0
        })
        .await;
    assert_eq!(open["open"], 0);
    shell
        .wait_for("document.querySelector('.statusbar .status')?.textContent === 'Live'")
        .await;

    // A renderer that stops acknowledging gets at most WINDOW items, then
    // the host ends the stream with `null` and frees the slot. Each added
    // job is one event; the jobs fail fast on a missing file.
    let before = shell.notices.all().len();
    let stalled_stream = shell
        .ipc(&format!(
            "const hello = await client.hello();
             const received = [];
             const channel = new core.Channel();
             channel.onmessage = (m) => received.push(m === null ? null : m.type);
             await core.invoke('subscribe', {{ after: hello.head, channel }});
             const items = Array.from({{ length: {count} }}, (_, i) => '{missing}?' + i);
             await client.submit({{ idempotencyKey: crypto.randomUUID(), target: hello.instance, items }});
             await new Promise((resolve) => setTimeout(resolve, 2000));
             const held = received.length;
             await new Promise((resolve) => {{
               const check = () => received.includes(null) ? resolve() : setTimeout(check, 200);
               check();
             }});
             return {{ held, total: received.length, last: received.at(-1) }};",
            count = WINDOW * 2,
            missing = server.url("/missing"),
        ))
        .await;
    assert_eq!(stalled_stream["held"], WINDOW, "{stalled_stream}");
    assert_eq!(stalled_stream["total"], WINDOW + 1);
    assert_eq!(stalled_stream["last"], Value::Null);
    shell
        .notices
        .wait(before, |n| {
            n["type"] == "subscriptions" && n["ended"] == "stalled"
        })
        .await;
    // The page's own feed kept up throughout and shows every job.
    shell
        .wait_for(&format!(
            "document.querySelector('.statusbar .status')?.textContent === 'Live'
             && document.querySelector('button[aria-label=\"All ({})\"]')",
            WINDOW * 2 + 1
        ))
        .await;

    let printed = shell.close().await;
    assert!(
        printed
            .iter()
            .any(|n| n["type"] == "stopped" && n["clean"] == true)
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs an X display"]
async fn runs_without_a_daemon_or_listener() {
    let dirs = Dirs::new().await;
    let mut app = Command::new(BIN)
        .args(dirs.run_args())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let notices = Notices::default();
    notices.collect(app.stdout.take().unwrap());
    // The page loaded and subscribed over IPC.
    notices
        .wait(0, |n| n["type"] == "subscriptions" && n["open"] == 1)
        .await;
    let pid = app.id().unwrap();
    assert_embedded(pid, &[]);

    close_window(pid);
    let status = tokio::time::timeout(DEADLINE, app.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());
    let stopped = notices.wait(0, |n| n["type"] == "stopped").await;
    assert_eq!(stopped["clean"], true);
}
