//! Hard-kill recovery over real sockets (architecture brief section 13.2,
//! "crash-injection harness"): an engine runs in a child process against
//! the local HTTPS fixture and is killed with `SIGKILL` while streaming, at
//! each checkpoint step, and at each publication step. An engine restarted
//! on the same state directory must take it over, resume only from the
//! durable bytes, complete the interrupted publication, and publish exactly
//! the served bytes without overwriting anything.
//!
//! Brief tests exercised here: T23 (crash before and after the file sync
//! and the checkpoint commit), T24 (crash before and after the link and the
//! completion commit), T26 (the killed engine held the state directory
//! until it died, and nothing had to clean its lock up), and T31 (a file
//! that took the target name meanwhile is never overwritten). Repeated
//! kills while recovery abandons a partial and while cleanup removes it
//! show that cleanup is safe to interrupt and repeat (section 7.6).
//!
//! The child is this test binary running [`child_engine`]. It stops at a
//! named boundary of the engine's `failpoints` feature and reports it, so
//! every kill lands on a known step rather than on a timer.

#![cfg(unix)]

#[path = "../../engine/tests/support/mod.rs"]
mod engine_support;
mod support;

use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use engine_support::{KIB, body, complete, jobs, settled, shutdown, staging_files};
use support::{Resource, Server, config_trusting, https_env};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use wayfetch_engine::fail;
use wayfetch_engine::state_dir::{StateDir, StateDirError};
use wayfetch_net::{CertificateDer, HttpTransport};
use wayfetch_protocol::command::IdempotencyKey;
use wayfetch_protocol::domain::job::JobStage;
use wayfetch_protocol::query::JobSummary;
use wayfetch_protocol::{Command, CommandEnvelope, CommandResult};

type Env = engine_support::Env<Server>;

/// Prefixes every line the child reports on its standard output.
const REPORT: &str = "@@wayfetch-child ";

/// Environment variables that turn [`child_engine`] on and configure it.
const CHILD_STATE: &str = "WAYFETCH_CHILD_STATE";
const CHILD_ROOT: &str = "WAYFETCH_CHILD_ROOT";
const CHILD_CA: &str = "WAYFETCH_CHILD_CA";
/// Submitted at start; without it the child takes over the state
/// directory's only job.
const CHILD_URL: &str = "WAYFETCH_CHILD_URL";
/// `<boundary>:<n>`: stop at the `n`th pass through the boundary.
const CHILD_STOP: &str = "WAYFETCH_CHILD_STOP";
/// `<idempotency key> <name>`: once the job is complete, rename its file.
const CHILD_RENAME: &str = "WAYFETCH_CHILD_RENAME";

/// How long any one wait for the child may take.
const DEADLINE: Duration = Duration::from_secs(20);

/// The engine process the tests start and kill. It downloads one URL on the
/// parent's directories, or resumes the job a killed child left, reports
/// durable progress, and with a stop boundary blocks there for good after
/// reporting it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "run in a child process by the other tests"]
async fn child_engine() {
    let Some(state) = std::env::var_os(CHILD_STATE) else {
        return;
    };
    let var = |name| std::env::var(name).unwrap();
    if let Ok(stop) = std::env::var(CHILD_STOP) {
        let (boundary, n) = stop.split_once(':').unwrap();
        stop_at(boundary.to_owned(), n.parse().unwrap());
    }
    let ca = CertificateDer::from(std::fs::read(var(CHILD_CA)).unwrap());
    let transport = HttpTransport::new(config_trusting(Some(ca))).unwrap();
    let env = engine_support::Env::at(
        state.into(),
        var(CHILD_ROOT).into(),
        (),
        Arc::new(transport),
    );
    let owner = env.start().await;
    let ids = match std::env::var(CHILD_URL) {
        Ok(url) => engine_support::submit(&owner, [url]).await,
        Err(_) => jobs(&owner).await.iter().map(|j| j.id).collect(),
    };
    let [id] = ids[..] else { panic!("one job") };
    let mut rename = std::env::var(CHILD_RENAME).ok();
    let mut reported = None;
    loop {
        let job = engine_support::job(&owner, id).await;
        if reported != Some(job.bytes_done) {
            println!("{REPORT}durable {}", job.bytes_done);
            reported = Some(job.bytes_done);
        }
        if job.stage == JobStage::Complete
            && let Some(rename) = rename.take()
        {
            let (key, name) = rename.split_once(' ').unwrap();
            let envelope = renaming(&owner, &job, key.parse().unwrap(), name);
            // A stopped rename holds the engine; the kill ends it.
            let handle = owner.handle();
            tokio::spawn(async move { handle.execute(envelope, engine_support::ALL).await });
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// A rename of `job`'s file to `name` under `key`.
fn renaming(
    owner: &wayfetch_engine::EngineOwner,
    job: &JobSummary,
    key: uuid::Uuid,
    name: &str,
) -> CommandEnvelope {
    CommandEnvelope {
        idempotency_key: IdempotencyKey::from_uuid(key),
        target: owner.identity().instance,
        command: Command::Rename {
            job: job.id,
            expected_revision: job.revision,
            name: name.into(),
        },
    }
}

/// Blocks the thread that makes the `n`th pass through `boundary`, after
/// reporting it, until the process is killed.
fn stop_at(boundary: String, n: usize) {
    let passes = AtomicUsize::new(0);
    let name = boundary.clone();
    fail::cfg_callback(boundary, move || {
        if passes.fetch_add(1, Ordering::SeqCst) + 1 == n {
            println!("{REPORT}stopped {name}");
            loop {
                std::thread::park();
            }
        }
    })
    .unwrap();
}

/// A running child engine and what it reported so far.
struct Child {
    process: tokio::process::Child,
    reports: mpsc::UnboundedReceiver<String>,
    /// Durable bytes the child last reported.
    durable: u64,
    /// The boundary the child stopped at.
    stopped: Option<String>,
}

impl Child {
    /// Starts an engine in a child process on `env`'s directories that
    /// downloads `path`, or without one resumes the job already there,
    /// stopping at the `n`th pass through `boundary`.
    fn spawn(env: &Env, path: Option<&str>, stop: Option<(&str, usize)>) -> Self {
        Self::spawn_renaming(env, path, stop, None)
    }

    /// [`Child::spawn`], renaming the job's file once it is complete under
    /// an idempotency key to a name.
    fn spawn_renaming(
        env: &Env,
        path: Option<&str>,
        stop: Option<(&str, usize)>,
        rename: Option<(uuid::Uuid, &str)>,
    ) -> Self {
        let ca = env.state.with_file_name("ca.der");
        std::fs::write(&ca, env.server.ca()).unwrap();
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["child_engine", "--exact", "--ignored", "--nocapture"])
            .env(CHILD_STATE, &env.state)
            .env(CHILD_ROOT, &env.root)
            .env(CHILD_CA, &ca)
            .env_remove(CHILD_URL)
            .env_remove(CHILD_STOP)
            .env_remove(CHILD_RENAME)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .kill_on_drop(true);
        if let Some(path) = path {
            command.env(CHILD_URL, env.server.url(path));
        }
        if let Some((boundary, n)) = stop {
            command.env(CHILD_STOP, format!("{boundary}:{n}"));
        }
        if let Some((key, name)) = rename {
            command.env(CHILD_RENAME, format!("{key} {name}"));
        }
        let mut process = command.spawn().unwrap();
        let mut lines = BufReader::new(process.stdout.take().unwrap()).lines();
        let (send, reports) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                // The test harness may print on the same line.
                if let Some((_, report)) = line.split_once(REPORT) {
                    let _ = send.send(report.to_owned());
                }
            }
        });
        Self {
            process,
            reports,
            durable: 0,
            stopped: None,
        }
    }

    /// Reads reports until `done` holds, rechecking it at least every few
    /// milliseconds for conditions outside the reports.
    async fn until(&mut self, done: impl Fn(&Self) -> bool) {
        let deadline = tokio::time::Instant::now() + DEADLINE;
        while !done(self) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out: durable {}, stopped {:?}",
                self.durable,
                self.stopped
            );
            match tokio::time::timeout(Duration::from_millis(5), self.reports.recv()).await {
                Err(_) => {}
                Ok(None) => panic!("the child exited: {:?}", self.process.try_wait()),
                Ok(Some(report)) => match report.split_once(' ').unwrap() {
                    ("durable", n) => self.durable = n.parse().unwrap(),
                    ("stopped", boundary) => self.stopped = Some(boundary.to_owned()),
                    other => panic!("unknown report {other:?}"),
                },
            }
        }
    }

    /// Kills the child with `SIGKILL`, after checking that it still owns
    /// `state`, and reaps it.
    async fn kill(mut self, state: &Path) {
        assert!(matches!(
            StateDir::open(state),
            Err(StateDirError::Locked(_))
        ));
        self.process.start_kill().unwrap();
        let status = self.process.wait().await.unwrap();
        assert_eq!(status.signal(), Some(SIGKILL), "{status:?}");
    }
}

/// The signal number of `SIGKILL` on every Unix.
const SIGKILL: i32 = 9;

/// Lengths of the staging files in `root`.
fn staging_lengths(root: &Path) -> Vec<u64> {
    std::fs::read_dir(root.join(".wayfetch/staging"))
        .map(|entries| {
            entries
                .map(|e| e.unwrap().metadata().unwrap().len())
                .collect()
        })
        .unwrap_or_default()
}

/// The durable bytes the store had committed for the job's latest partial,
/// read from the database the killed engine left.
fn committed_bytes(state: &Path) -> u64 {
    let db = rusqlite::Connection::open_with_flags(
        state.join("state.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    db.query_row(
        "SELECT coalesce(max(durable_bytes), 0) FROM partials \
         WHERE id = (SELECT max(id) FROM partials)",
        [],
        |row| row.get::<_, i64>(0),
    )
    .unwrap()
    .try_into()
    .unwrap()
}

async fn env() -> Env {
    https_env(|_| {}).await
}

/// Staging records the killed engine left that wait for cleanup.
fn pending_cleanups(state: &Path) -> i64 {
    let db = rusqlite::Connection::open_with_flags(
        state.join("state.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    db.query_row(
        "SELECT count(*) FROM staging_files WHERE cleanup = 'pending'",
        [],
        |row| row.get(0),
    )
    .unwrap()
}

/// Restarts an engine on `env` and waits for its only job to complete.
async fn restart(env: &Env) -> (wayfetch_engine::EngineOwner, JobSummary) {
    let owner = env.start().await;
    let [job] = &jobs(&owner).await[..] else {
        panic!("one job")
    };
    let done = complete(&owner, job.id).await;
    // Recovery took the job over under a new generation.
    assert!(done.generation.get() > 1, "{done:?}");
    (owner, done)
}

/// Checks that `done` published `data` at `name` and left no staging file.
fn assert_published(env: &Env, done: &JobSummary, name: &str, data: &Bytes) {
    let artifact = done.artifact.as_ref().expect("published");
    assert_eq!(
        (artifact.path.as_str(), artifact.size),
        (name, data.len() as u64)
    );
    assert_eq!(env.published(name), *data);
    assert_eq!(staging_files(&env.root), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_download_killed_while_streaming_resumes_from_its_durable_bytes() {
    let env = env().await;
    let data = body(200 * KIB + 11);
    // Not on a checkpoint boundary, so written bytes can outrun durable ones.
    let at = 72 * KIB + 1000;
    env.server.put(
        "/big.bin",
        Resource {
            stall_at: Some(at),
            ..Resource::new(data.clone())
        },
    );
    let mut child = Child::spawn(&env, Some("/big.bin"), None);
    // Every byte before the stall is in the staging file and the last
    // checkpoint committed; the response is still open.
    child
        .until(|c| settled(c.durable, at) && staging_lengths(&env.root) == [at as u64])
        .await;
    child.kill(&env.state).await;
    env.server.update("/big.bin", |r| r.stall_at = None);
    let durable = committed_bytes(&env.state);
    assert!(durable > 0 && durable <= at as u64, "{durable}");

    let (owner, done) = restart(&env).await;
    // The rest was requested from the durable bytes on, conditioned on the
    // representation; the file's length did not count as coverage.
    assert_eq!(
        env.server.ranges("/big.bin"),
        vec![None, Some((durable, None))]
    );
    assert_eq!(env.server.log()[1].header("if-range"), Some("\"v1\""));
    assert_published(&env, &done, "big.bin", &data);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_download_killed_at_a_checkpoint_resumes_from_committed_bytes_only() {
    // Each boundary stops the third checkpoint. Before its sync the
    // coordinator still commits earlier ones, so the test waits for one to
    // be reported; the other two hold the coordinator after two committed.
    let cases = [
        ("checkpoint-before-sync", true),
        ("checkpoint-before-commit", false),
        ("checkpoint-after-commit", false),
    ];
    for (boundary, report_progress) in cases {
        let env = env().await;
        let data = body(64 * KIB);
        env.server.put("/c.bin", Resource::new(data.clone()));
        let mut child = Child::spawn(&env, Some("/c.bin"), Some((boundary, 3)));
        child
            .until(|c| c.stopped.is_some() && (c.durable > 0 || !report_progress))
            .await;
        assert_eq!(child.stopped.as_deref(), Some(boundary));
        child.kill(&env.state).await;
        let written = staging_lengths(&env.root);
        let durable = committed_bytes(&env.state);
        let [written] = written[..] else {
            panic!("{boundary}: {written:?}")
        };
        assert!(durable > 0 && durable <= written, "{boundary}: {durable}");
        if boundary != "checkpoint-after-commit" {
            // The stopped checkpoint's bytes were written but never
            // committed.
            assert!(durable < written, "{boundary}: {durable} of {written}");
        }

        let (owner, done) = restart(&env).await;
        assert_eq!(
            env.server.ranges("/c.bin"),
            vec![None, Some((durable, None))],
            "{boundary}"
        );
        assert_published(&env, &done, "c.bin", &data);
        shutdown(owner).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_publication_killed_at_any_step_completes_after_restart() {
    for boundary in [
        // The intent is recorded; the staging file has no other name.
        "publish-before-link",
        // Both names are the staged file.
        "publish-before-unlink",
        // Only the target name is left; the completion is not committed.
        "publish-before-complete",
    ] {
        let env = env().await;
        let data = body(48 * KIB + 5);
        env.server.put("/p.bin", Resource::new(data.clone()));
        let mut child = Child::spawn(&env, Some("/p.bin"), Some((boundary, 1)));
        child.until(|c| c.stopped.is_some()).await;
        child.kill(&env.state).await;

        let (owner, done) = restart(&env).await;
        // Reconciliation finished the publication from the staged file;
        // nothing was downloaded again.
        assert_eq!(env.server.hits("/p.bin"), 1, "{boundary}");
        assert_published(&env, &done, "p.bin", &data);
        shutdown(owner).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_that_took_the_name_during_a_crash_is_never_overwritten() {
    let env = env().await;
    let data = body(48 * KIB);
    env.server.put("/p.bin", Resource::new(data.clone()));
    let mut child = Child::spawn(&env, Some("/p.bin"), Some(("publish-before-link", 1)));
    child.until(|c| c.stopped.is_some()).await;
    child.kill(&env.state).await;
    // Someone else's file appears under the reserved name.
    let theirs = env.root.join("p.bin");
    std::fs::write(&theirs, b"someone else's file").unwrap();

    let (owner, done) = restart(&env).await;
    assert_eq!(std::fs::read(&theirs).unwrap(), b"someone else's file");
    assert_eq!(env.server.hits("/p.bin"), 1);
    assert_published(&env, &done, "p (1).bin", &data);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rename_killed_at_any_step_is_finished_or_undone_after_restart() {
    for (boundary, renamed) in [
        // The rename is recorded; the file has only its old name.
        ("rename-before-link", false),
        // Both names are the file.
        ("rename-before-unlink", true),
        // Only the new name is left; the rename is not committed.
        ("rename-before-commit", true),
    ] {
        let env = env().await;
        let data = body(20 * KIB);
        env.server.put("/n.bin", Resource::new(data.clone()));
        let key = uuid::Uuid::new_v4();
        let mut child = Child::spawn_renaming(
            &env,
            Some("/n.bin"),
            Some((boundary, 1)),
            Some((key, "m.bin")),
        );
        child.until(|c| c.stopped.is_some()).await;
        child.kill(&env.state).await;

        let owner = env.start().await;
        let [job] = &jobs(&owner).await[..] else {
            panic!("one job")
        };
        let (name, gone) = if renamed {
            ("m.bin", "n.bin")
        } else {
            ("n.bin", "m.bin")
        };
        assert_published(&env, job, name, &data);
        assert!(!env.root.join(gone).exists(), "{boundary}");
        // A retry under the same key replays the finished rename, or
        // renames now if the crash came before the link.
        let before = if renamed {
            wayfetch_protocol::domain::version::Revision::from_u64(job.revision.get() - 1).unwrap()
        } else {
            job.revision
        };
        let retry = renaming(
            &owner,
            &JobSummary {
                revision: before,
                ..job.clone()
            },
            key,
            "m.bin",
        );
        let result = owner
            .handle()
            .execute(retry, engine_support::ALL)
            .await
            .unwrap();
        let after = engine_support::job(&owner, job.id).await;
        assert_eq!(
            result,
            CommandResult::JobUpdated {
                job: job.id,
                revision: after.revision,
                changed: true
            },
            "{boundary}"
        );
        assert_published(&env, &after, "m.bin", &data);
        assert!(!env.root.join("n.bin").exists());
        shutdown(owner).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cleanup_of_an_abandoned_partial_survives_repeated_kills() {
    let env = env().await;
    let data = body(64 * KIB + 7);
    let at = 20 * KIB + 300;
    env.server.put(
        "/r.bin",
        Resource {
            stall_at: Some(at),
            ..Resource::new(data.clone())
        },
    );
    let mut child = Child::spawn(&env, Some("/r.bin"), None);
    child
        .until(|c| settled(c.durable, at) && staging_lengths(&env.root) == [at as u64])
        .await;
    child.kill(&env.state).await;
    let durable = committed_bytes(&env.state);
    // The server no longer honors ranges, so the partial cannot continue.
    env.server.update("/r.bin", |r| {
        r.stall_at = None;
        r.ranges = false;
    });

    // Recovery abandons the partial and queues the job to start from zero;
    // killed as cleanup is about to remove the file.
    let mut child = Child::spawn(&env, None, Some(("cleanup-before-unlink", 1)));
    child.until(|c| c.stopped.is_some()).await;
    child.kill(&env.state).await;
    assert_eq!(staging_lengths(&env.root), [at as u64]);
    assert_eq!(pending_cleanups(&env.state), 1);

    // The next start's cleanup removes the file; killed before it forgets
    // the record.
    let mut child = Child::spawn(&env, None, Some(("cleanup-before-forget", 1)));
    child.until(|c| c.stopped.is_some()).await;
    child.kill(&env.state).await;
    assert_eq!(staging_lengths(&env.root), Vec::<u64>::new());
    assert_eq!(pending_cleanups(&env.state), 1);

    // The last start finds the record without its file, forgets it, and
    // downloads the job from zero.
    let (owner, done) = restart(&env).await;
    assert_eq!(pending_cleanups(&env.state), 0);
    assert_eq!(
        env.server.ranges("/r.bin"),
        vec![None, Some((durable, None)), None]
    );
    assert_published(&env, &done, "r.bin", &data);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_left_unregistered_by_a_kill_is_never_removed() {
    let env = env().await;
    let data = body(16 * KIB);
    env.server.put("/u.bin", Resource::new(data.clone()));
    let mut child = Child::spawn(&env, Some("/u.bin"), Some(("staging-before-register", 1)));
    child.until(|c| c.stopped.is_some()).await;
    child.kill(&env.state).await;
    // The file the killed engine created but never recorded is replaced
    // before the next start; only a record could prove it the engine's.
    let staging = env.root.join(".wayfetch/staging");
    let [left] = &std::fs::read_dir(&staging)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect::<Vec<_>>()[..]
    else {
        panic!("one staging file")
    };
    let swap = staging.join("swap");
    std::fs::write(&swap, b"someone else's file").unwrap();
    std::fs::rename(&swap, left).unwrap();

    let (owner, done) = restart(&env).await;
    assert_eq!(done.artifact.as_ref().unwrap().path, "u.bin");
    assert_eq!(env.published("u.bin"), data);
    assert_eq!(std::fs::read(left).unwrap(), b"someone else's file");
    assert_eq!(staging_files(&env.root), 1);
    shutdown(owner).await;
}
