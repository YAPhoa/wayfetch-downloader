//! The `wayfetchd` image run through `deploy/compose.yaml`, with every
//! restriction it documents: a configured non-root UID/GID, a read-only
//! root filesystem, no capabilities, and separate state, download, and
//! token mounts. A fixture download survives the container's removal and
//! recreation and resumes from its durable bytes; `docker compose stop`
//! shuts down gracefully; a second container cannot take a state directory
//! in use; and a root that was never initialized, or a host path that does
//! not exist, is refused rather than written into.
//!
//! These tests need Docker with the Compose plugin, so they are ignored by
//! default:
//!
//! ```sh
//! cargo test --locked -p wayfetchd --test container -- --ignored
//! ```
//!
//! They build `wayfetch:local` with `docker compose build`, unless
//! `WAYFETCH_IMAGE` names an image to test instead. The fixture server
//! listens on the host's Docker bridge address, which containers reach.

mod common;
#[path = "../../../crates/engine/tests/support/mod.rs"]
mod engine_support;
#[path = "../../../crates/net/tests/support/mod.rs"]
mod support;

use std::net::IpAddr;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use common::{Client, DEADLINE, SETTLED, STALL, release, stalled};
use reqwest::StatusCode;
use serde_json::{Value, json};
use support::Server;

const REPO: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
/// The identity the stack runs as when the tests run as root: the default
/// in `deploy/compose.yaml`.
const DEPLOYMENT_ID: u32 = 568;

/// The image under test, built once per test run.
fn image() -> &'static str {
    static IMAGE: OnceLock<String> = OnceLock::new();
    IMAGE.get_or_init(|| {
        if let Ok(image) = std::env::var("WAYFETCH_IMAGE") {
            return image;
        }
        let out = Command::new("docker")
            .args(["compose", "-f", &compose_file(), "build", "--quiet"])
            .env("WAYFETCH_IMAGE", "wayfetch:local")
            // Paths are required to read the file, not to build.
            .envs(["WAYFETCH_STATE", "WAYFETCH_DOWNLOADS", "WAYFETCH_TOKENS"].map(|v| (v, "/")))
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", lossy(&out.stderr));
        "wayfetch:local".to_owned()
    })
}

fn compose_file() -> String {
    format!("{REPO}/deploy/compose.yaml")
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Runs `docker ARGS` and returns its trimmed standard output.
fn docker(args: &[&str]) -> String {
    let out = Command::new("docker").args(args).output().unwrap();
    assert!(
        out.status.success(),
        "docker {args:?}: {}",
        lossy(&out.stderr)
    );
    lossy(&out.stdout).trim().to_owned()
}

/// The host's address on Docker's default bridge.
fn bridge_gateway() -> IpAddr {
    docker(&[
        "network",
        "inspect",
        "bridge",
        "--format",
        "{{(index .IPAM.Config 0).Gateway}}",
    ])
    .parse()
    .unwrap()
}

/// One Compose project: its host directories and the fixture's CA.
struct Stack {
    project: String,
    tmp: tempfile::TempDir,
    state: PathBuf,
    downloads: PathBuf,
    tokens: PathBuf,
    uid: u32,
    gid: u32,
}

impl Stack {
    /// Fresh, empty host directories owned by the identity the container
    /// runs as, and an override that trusts `server` and allows its
    /// address, as an operator would for a server on their network.
    fn new(server: &Server, gateway: IpAddr) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let me = std::fs::metadata(tmp.path()).unwrap();
        // Root runs the stack as the deployment default; anyone else as
        // themselves, since they cannot hand directories to another user.
        let (uid, gid) = match me.uid() {
            0 => (DEPLOYMENT_ID, DEPLOYMENT_ID),
            uid => (uid, me.gid()),
        };
        let stack = Self {
            project: format!(
                "wayfetch-test-{}",
                &uuid::Uuid::new_v4().simple().to_string()[..12]
            ),
            state: tmp.path().join("state"),
            downloads: tmp.path().join("downloads"),
            tokens: tmp.path().join("tokens"),
            tmp,
            uid,
            gid,
        };
        for dir in [&stack.state, &stack.downloads, &stack.tokens] {
            std::fs::create_dir(dir).unwrap();
            std::os::unix::fs::chown(dir, Some(uid), Some(gid)).unwrap();
        }
        let fixture = stack.tmp.path().join("fixture");
        std::fs::create_dir(&fixture).unwrap();
        std::fs::write(fixture.join("ca.der"), server.ca()).unwrap();
        set_mode(&fixture, 0o755);
        set_mode(&fixture.join("ca.der"), 0o644);

        // The image's own command, plus the fixture's exceptions.
        let mut command: Vec<String> = serde_json::from_str(&docker(&[
            "image",
            "inspect",
            image(),
            "--format",
            "{{json .Config.Cmd}}",
        ]))
        .unwrap();
        command.extend([
            "--allow-network".to_owned(),
            format!("{gateway}/32"),
            "--ca-cert".to_owned(),
            "/fixture/ca.der".to_owned(),
        ]);
        let compose_override = json!({
            "services": {"wayfetchd": {
                "command": command,
                "volumes": [{
                    "type": "bind",
                    "source": fixture,
                    "target": "/fixture",
                    "read_only": true,
                }],
            }},
        });
        std::fs::write(stack.override_file(), compose_override.to_string()).unwrap();
        stack
    }

    fn override_file(&self) -> PathBuf {
        self.tmp.path().join("compose.override.json")
    }

    /// `docker compose` for this project with `downloads` as the download
    /// root.
    fn compose_with(&self, downloads: &Path, args: &[&str]) -> Output {
        Command::new("docker")
            .args(["compose", "-f", &compose_file(), "-f"])
            .arg(self.override_file())
            .args(["-p", &self.project])
            .args(args)
            .env("WAYFETCH_IMAGE", image())
            .env("WAYFETCH_UID", self.uid.to_string())
            .env("WAYFETCH_GID", self.gid.to_string())
            .env("WAYFETCH_STATE", &self.state)
            .env("WAYFETCH_DOWNLOADS", downloads)
            .env("WAYFETCH_TOKENS", &self.tokens)
            // Any free port on the host's loopback.
            .env("WAYFETCH_PUBLISH", "127.0.0.1:0")
            .output()
            .unwrap()
    }

    fn compose(&self, args: &[&str]) -> Output {
        self.compose_with(&self.downloads, args)
    }

    /// `docker compose ARGS`, which must succeed; returns standard output.
    fn ok(&self, args: &[&str]) -> String {
        let out = self.compose(args);
        assert!(
            out.status.success(),
            "compose {args:?}: {}{}",
            lossy(&out.stderr),
            self.logs()
        );
        lossy(&out.stdout).trim().to_owned()
    }

    fn init_root(&self) {
        let out = self.ok(&[
            "run",
            "--rm",
            "--no-deps",
            "-T",
            "wayfetchd",
            "init-root",
            "--state-dir",
            "/state",
            "/downloads",
        ]);
        assert!(
            out.contains("initialized download root /downloads"),
            "{out}"
        );
    }

    /// Starts the service and waits until its API answers. Returns its
    /// address on the host.
    async fn start(&self) -> String {
        self.ok(&["up", "--detach", "--quiet-pull"]);
        let address = self.ok(&["port", "wayfetchd", "7878"]);
        let base = format!("http://{address}");
        let anonymous = Client::new(&base, "", None);
        let deadline = Instant::now() + DEADLINE;
        while anonymous
            .http
            .get(format!("{base}/v1/ready"))
            .send()
            .await
            .is_err()
        {
            assert!(Instant::now() < deadline, "no answer:\n{}", self.logs());
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        base
    }

    /// Starts the service, waits until Docker reports it healthy, and
    /// returns a client with the token it generated.
    async fn up(&self) -> Client {
        let base = self.start().await;
        let deadline = Instant::now() + DEADLINE;
        while self.health() != "healthy" {
            assert!(Instant::now() < deadline, "not healthy:\n{}", self.logs());
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let mut client = Client::new(&base, "", Some(&self.token()));
        let (status, hello) = client.get("/v1/capabilities").await;
        assert_eq!(status, StatusCode::OK, "{hello}");
        client.instance = hello["instance"].as_str().unwrap().to_owned();
        client
    }

    /// The token the daemon generated in the token mount.
    fn token(&self) -> String {
        let text = std::fs::read_to_string(self.tokens.join("api-tokens")).unwrap();
        let line = text.lines().find(|l| !l.starts_with('#')).unwrap();
        line.split_once(' ').unwrap().0.to_owned()
    }

    fn container(&self) -> String {
        self.ok(&["ps", "--all", "--quiet", "wayfetchd"])
    }

    fn inspect(&self, format: &str) -> String {
        docker(&["inspect", &self.container(), "--format", format])
    }

    fn health(&self) -> String {
        self.inspect("{{.State.Health.Status}}")
    }

    fn logs(&self) -> String {
        lossy(&self.compose(&["logs", "--no-color"]).stdout)
    }

    /// `wayfetchd ready` inside the running container, as the health check
    /// runs it.
    fn probe(&self) -> Output {
        self.compose(&[
            "exec",
            "-T",
            "wayfetchd",
            "/usr/local/bin/wayfetchd",
            "ready",
        ])
    }

    /// Stops the service with SIGTERM and checks it shut down cleanly well
    /// within the grace period.
    fn stop(&self) {
        let started = Instant::now();
        self.ok(&["stop"]);
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(self.inspect("{{.State.ExitCode}}"), "0", "{}", self.logs());
        let logs = self.logs();
        assert!(
            logs.contains("stopped; unfinished jobs continue on the next start"),
            "{logs}"
        );
    }

    /// Checks the restrictions the Compose file promises.
    fn assert_restricted(&self) {
        let config = self.inspect(
            "{{.Config.User}} {{.HostConfig.ReadonlyRootfs}} {{json .HostConfig.CapDrop}} \
             {{json .HostConfig.SecurityOpt}} {{.HostConfig.Init}} {{json .HostConfig.Tmpfs}} \
             {{.HostConfig.PidsLimit}}",
        );
        assert_eq!(
            config,
            format!(
                "{}:{} true [\"ALL\"] [\"no-new-privileges:true\"] true \
                 {{\"/tmp\":\"size=16m,mode=1777\"}} 128",
                self.uid, self.gid
            )
        );
        // Only the mounts are writable.
        let out = self.compose(&[
            "exec",
            "-T",
            "wayfetchd",
            "/usr/local/bin/wayfetchd",
            "init-root",
            "--state-dir",
            "/new-state",
            "/downloads",
        ]);
        assert!(!out.status.success());
        assert!(
            lossy(&out.stderr).contains("Read-only file system"),
            "{out:?}"
        );
    }
}

impl Drop for Stack {
    fn drop(&mut self) {
        let _ = self.compose(&["down", "--timeout", "45", "--remove-orphans"]);
    }
}

fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker with Compose"]
async fn a_recreated_container_resumes_and_publishes() {
    let gateway = bridge_gateway();
    let server = Server::https_on(gateway).await;
    let data = stalled(&server, "/r.bin");
    let stack = Stack::new(&server, gateway);
    stack.init_root();
    let client = stack.up().await;
    stack.assert_restricted();
    let id = client.submit(&server.url("/r.bin")).await;
    let settled = client.settled(&id).await;
    assert!(settled >= SETTLED);

    // SIGTERM stops it cleanly, and the container is removed entirely.
    let first = stack.container();
    stack.stop();
    stack.ok(&["down"]);
    assert!(stack.compose(&["ps", "--all", "--quiet"]).stdout.is_empty());

    // A new container on the same mounts is the same instance and resumes
    // from the durable bytes rather than from zero.
    release(&server, "/r.bin");
    let again = stack.up().await;
    assert_ne!(stack.container(), first);
    assert_eq!(again.instance, client.instance);
    let done = again.stage(&id, "complete").await;
    assert!(done["generation"].as_u64().unwrap() > 1, "{done}");
    let published = stack.downloads.join("r.bin");
    assert_eq!(std::fs::read(&published).unwrap(), data);
    let owner = std::fs::metadata(&published).unwrap();
    assert_eq!((owner.uid(), owner.gid()), (stack.uid, stack.gid));
    let ranges = server.ranges("/r.bin");
    let [None, Some((from, None))] = ranges[..] else {
        panic!("{ranges:?}")
    };
    assert!((settled..=STALL as u64).contains(&from), "{from}");
    stack.stop();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker with Compose"]
async fn a_second_container_cannot_take_the_state_directory() {
    let gateway = bridge_gateway();
    let server = Server::https_on(gateway).await;
    let stack = Stack::new(&server, gateway);
    stack.init_root();
    let client = stack.up().await;

    for args in [
        &["run", "--rm", "--no-deps", "-T", "wayfetchd"][..],
        &[
            "run",
            "--rm",
            "--no-deps",
            "-T",
            "wayfetchd",
            "init-root",
            "--state-dir",
            "/state",
            "/downloads",
        ],
    ] {
        let out = stack.compose(args);
        assert!(!out.status.success(), "{args:?}");
        let stderr = lossy(&out.stderr);
        assert!(
            stderr.contains("owned by another running engine"),
            "{stderr}"
        );
    }
    let (status, _) = client.get("/v1/jobs").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(stack.health(), "healthy");
    stack.stop();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Docker with Compose"]
async fn a_missing_root_is_refused_not_written_into() {
    let gateway = bridge_gateway();
    let server = Server::https_on(gateway).await;
    let stack = Stack::new(&server, gateway);

    // A host path that does not exist stops Compose, which creates nothing.
    let missing = stack.tmp.path().join("missing");
    let out = stack.compose_with(&missing, &["up", "--detach"]);
    assert!(!out.status.success());
    assert!(
        lossy(&out.stderr).contains("bind source path does not exist"),
        "{out:?}"
    );
    assert!(!missing.exists());
    stack.ok(&["down"]);

    // An existing directory that was never initialized, such as an
    // unmounted dataset's empty mount point: the daemon serves but is not
    // ready, and a download waits instead of writing there.
    let base = stack.start().await;
    let client = Client::new(&base, "", Some(&stack.token()));
    let anonymous = Client::new(&base, "", None);
    let (status, ready) = anonymous.get("/v1/ready").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        ready,
        json!({"ready": false, "reason": "download_root_unavailable"})
    );
    let out = stack.probe();
    assert!(!out.status.success());
    assert!(
        lossy(&out.stderr).contains("not ready: download_root_unavailable"),
        "{out:?}"
    );
    assert_ne!(stack.health(), "healthy");

    let (_, hello) = client.get("/v1/capabilities").await;
    let client = Client {
        instance: hello["instance"].as_str().unwrap().to_owned(),
        ..client
    };
    server.put("/a.bin", support::Resource::new(engine_support::body(1024)));
    let id = client.submit(&server.url("/a.bin")).await;
    let deadline = Instant::now() + DEADLINE;
    loop {
        let job: Value = client.job(&id).await;
        if job["stage"] == "blocked" {
            break;
        }
        assert!(Instant::now() < deadline, "{job}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(std::fs::read_dir(&stack.downloads).unwrap().count(), 0);
    stack.stop();
}

#[test]
#[ignore = "needs Docker"]
fn the_image_trusts_the_system_certificate_authorities() {
    let container = docker(&["create", image()]);
    let tmp = tempfile::tempdir().unwrap();
    let bundle = tmp.path().join("ca-certificates.crt");
    let copied = Command::new("docker")
        .arg("cp")
        .arg(format!("{container}:/etc/ssl/certs/ca-certificates.crt"))
        .arg(&bundle)
        .output()
        .unwrap();
    docker(&["rm", &container]);
    assert!(copied.status.success(), "{}", lossy(&copied.stderr));
    let text = std::fs::read_to_string(&bundle).unwrap();
    let certificates = text.matches("-----BEGIN CERTIFICATE-----").count();
    assert!(certificates > 100, "{certificates}");
}

/// The build image's Rust tag is the toolchain `rust-toolchain.toml` pins,
/// so the image is built with the same compiler as everything else.
#[test]
fn the_image_builds_with_the_pinned_toolchain() {
    let read = |name: &str| std::fs::read_to_string(format!("{REPO}/{name}")).unwrap();
    let toolchain = read("rust-toolchain.toml");
    let channel = toolchain
        .lines()
        .find_map(|l| l.strip_prefix("channel = "))
        .unwrap()
        .trim_matches('"');
    let dockerfile = read("Dockerfile");
    let builder = dockerfile
        .lines()
        .find_map(|l| l.strip_prefix("ARG BUILDER_IMAGE="))
        .unwrap();
    assert!(
        builder.contains(&format!("/rust:{channel}-")),
        "{builder} does not use Rust {channel}"
    );
}
