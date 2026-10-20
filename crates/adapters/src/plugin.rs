//! Helper programs integrations run, such as yt-dlp, ffmpeg, and 7-Zip
//! (ADR-27).
//!
//! A program is found at a path the host configures, or on `PATH`, and
//! asked for its version once when the host starts: that answer is what
//! Settings > Plugins shows. Each run is a separate process started
//! without a shell, with no standard input unless the run gives it some
//! (a password, which so never appears on a command line), with its
//! output bounded and a deadline. Each run leads a process group that
//! whatever it starts joins; when the run ends, is stopped, or is dropped,
//! the whole group is killed, and on Linux a run returns only once none
//! of it still runs. A program that reads
//! untrusted input runs confined ([`Confinement`]), its version check
//! included, so a system that cannot confine it reports it not working.
//!
//! A [`Plugin`] is the program as Settings changes it: turned on or off,
//! and either the copy found that way or one Wayfetch installed in its
//! place ([`crate::plugins`]). Each run takes the copy in use when it
//! starts and keeps it to the end.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

pub use crate::confine::Confinement;
use wayfetch_engine::domain::name::{DiagnosticText, ProviderId};
use wayfetch_engine::protocol::media::{PluginRole, PluginState, PluginSummary};

/// Longest a version check may take.
const VERSION_DEADLINE: Duration = Duration::from_secs(20);

/// The most of a program's error output kept for its message.
const STDERR_BYTES: usize = 64 * 1024;

/// A program file named `name` in a directory on `PATH`, if any.
pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let names = if cfg!(windows) {
        vec![format!("{name}.exe"), name.to_owned()]
    } else {
        vec![name.to_owned()]
    };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .flat_map(|dir| names.iter().map(move |n| dir.join(n)))
        .find(|path| path.is_file())
}

/// What a plugin is, for Settings > Plugins.
#[derive(Debug, Clone, Copy)]
pub struct Describe {
    /// Its ID, stable across versions.
    pub id: &'static str,
    /// Its name.
    pub name: &'static str,
    /// What it does.
    pub role: PluginRole,
}

/// How a plugin's program is checked: the arguments that make it print
/// its version, how to read the version from what it printed, and the
/// environment every run of it gets.
#[derive(Debug, Clone)]
pub struct Check {
    /// The arguments.
    pub args: &'static [&'static str],
    /// Reads the version.
    pub version: fn(&str) -> Option<String>,
    /// Variables set for every run, unless the host's own environment
    /// sets them.
    pub env: Vec<(&'static str, OsString)>,
    /// How the check runs confined, if the program's runs are.
    pub confinement: Option<Confinement>,
}

/// A helper program and what its version check found.
#[derive(Debug, Clone)]
pub struct Program {
    path: Option<PathBuf>,
    state: PluginState,
    version: Option<String>,
    problem: Option<String>,
    env: Vec<(&'static str, OsString)>,
}

impl Program {
    /// Finds the program at `path` and checks it runs by asking it for its
    /// version as `check` says. No path means the program is missing.
    pub async fn detect(describe: Describe, path: Option<PathBuf>, check: &Check) -> Self {
        let mut program = Self {
            path,
            state: PluginState::Missing,
            version: None,
            problem: None,
            env: Vec::new(),
        };
        for (key, value) in &check.env {
            program = program.with_env(key, value.clone());
        }
        let Some(path) = &program.path else {
            program.problem = Some(format!("{} was not found.", describe.name));
            return program;
        };
        if !path.is_file() {
            program.problem = Some(format!(
                "{} was not found at {}.",
                describe.name,
                path.display()
            ));
            return program;
        }
        let limits = Limits {
            deadline: VERSION_DEADLINE,
            stdout_bytes: 64 * 1024,
        };
        match program
            .run_with(
                check.args,
                None,
                check.confinement.as_ref(),
                limits,
                std::future::pending(),
            )
            .await
        {
            Ok(output) => {
                program.state = PluginState::Ready;
                program.version = (check.version)(&String::from_utf8_lossy(&output.stdout));
            }
            Err(e) => {
                program.state = PluginState::Failed;
                program.problem = Some(format!("{} does not run: {e}", describe.name));
            }
        }
        program
    }

    /// Whether it runs.
    pub fn ready(&self) -> bool {
        self.state == PluginState::Ready
    }

    /// The version it reports, if known.
    pub fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    /// Why it cannot be used, if it cannot.
    pub fn problem(&self) -> Option<&str> {
        self.problem.as_deref()
    }

    /// The program run with `key` set to `value` in its environment,
    /// unless the host's own environment sets `key`.
    fn with_env(mut self, key: &'static str, value: impl Into<OsString>) -> Self {
        if std::env::var_os(key).is_none() {
            self.env.push((key, value.into()));
        }
        self
    }

    /// Where it is, if found.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Runs it with `args` and returns its output if it succeeded.
    pub async fn run<I, S>(&self, args: I, limits: Limits) -> Result<Output, RunError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run_with(args, None, None, limits, std::future::pending())
            .await
    }

    /// Runs it with `args`, writing `input` to its standard input, which
    /// is otherwise empty, under `confinement` if given, and returns its
    /// output if it succeeded. The input is not passed on a command line,
    /// so other processes cannot read it there. When `stop` resolves, the
    /// run is stopped: see [`Groups::end`].
    pub async fn run_with<I, S>(
        &self,
        args: I,
        input: Option<&[u8]>,
        confinement: Option<&Confinement>,
        limits: Limits,
        stop: impl Future<Output = ()>,
    ) -> Result<Output, RunError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = self.command(args, confinement)?;
        if input.is_some() {
            command.stdin(Stdio::piped());
        }
        let mut child = spawn(&mut command).await?;
        let groups = Groups::of([&child]);
        let (stdin, stdout, stderr) = (
            child.stdin.take(),
            child.stdout.take().ok_or(RunError::Missing)?,
            child.stderr.take().ok_or(RunError::Missing)?,
        );
        let ran = async {
            if let (Some(input), Some(mut stdin)) = (input, stdin) {
                // A program that never reads it closes the pipe: not an error.
                let _ = stdin.write_all(input).await;
            }
            tokio::try_join!(
                read(stdout, limits.stdout_bytes, true),
                read(stderr, STDERR_BYTES, false),
                wait(&mut child),
            )
        };
        let ran = bounded(ran, limits.deadline, stop).await;
        groups.end().await;
        let (stdout, stderr, status) = ran?;
        finished(status, stdout, stderr)
    }

    /// Runs it twice at once, with `from`'s standard output as `into`'s
    /// standard input, each under its confinement if given, and returns
    /// `into`'s output if both succeeded. Neither has other input. A
    /// failure is the first one's, if it failed, since the second then
    /// usually fails for want of input. When `stop` resolves, both are
    /// stopped: see [`Groups::end`].
    pub async fn pipe<I, J, S, T>(
        &self,
        (from, from_confinement): (I, Option<&Confinement>),
        (into, into_confinement): (J, Option<&Confinement>),
        limits: Limits,
        stop: impl Future<Output = ()>,
    ) -> Result<Output, RunError>
    where
        I: IntoIterator<Item = S>,
        J: IntoIterator<Item = T>,
        S: AsRef<OsStr>,
        T: AsRef<OsStr>,
    {
        let mut first = spawn(&mut self.command(from, from_confinement)?).await?;
        let between: Stdio = first
            .stdout
            .take()
            .ok_or(RunError::Missing)?
            .try_into()
            .map_err(|e: std::io::Error| RunError::Spawn(e.to_string()))?;
        let mut command = self.command(into, into_confinement)?;
        command.stdin(between);
        let mut second = spawn(&mut command).await?;
        let groups = Groups::of([&first, &second]);
        let (first_err, stdout, second_err) = (
            first.stderr.take().ok_or(RunError::Missing)?,
            second.stdout.take().ok_or(RunError::Missing)?,
            second.stderr.take().ok_or(RunError::Missing)?,
        );
        let ran = async {
            tokio::try_join!(
                read(first_err, STDERR_BYTES, false),
                read(stdout, limits.stdout_bytes, true),
                read(second_err, STDERR_BYTES, false),
                wait(&mut first),
                wait(&mut second),
            )
        };
        let ran = bounded(ran, limits.deadline, stop).await;
        groups.end().await;
        let (first_err, stdout, second_err, first_status, second_status) = ran?;
        finished(first_status, Vec::new(), first_err)?;
        finished(second_status, stdout, second_err)
    }

    /// The program as a command with `args` and its environment, or
    /// confined by `confinement`, without input, its output piped, and
    /// stopped when dropped.
    fn command<I, S>(
        &self,
        args: I,
        confinement: Option<&Confinement>,
    ) -> Result<tokio::process::Command, RunError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let path = self.path.as_ref().ok_or(RunError::Missing)?;
        let mut command = match confinement {
            Some(confinement) => confinement.command(path).map_err(RunError::Unconfined)?,
            None => {
                let mut command = tokio::process::Command::new(path);
                command
                    // A Python program writes UTF-8 whatever the host's locale.
                    .env("PYTHONIOENCODING", "utf-8")
                    .envs(self.env.iter().map(|(k, v)| (k, v)));
                command
            }
        };
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // It leads a process group of its own, which what it starts joins.
        #[cfg(unix)]
        command.process_group(0);
        #[cfg(windows)]
        // No console window opens for it on the desktop.
        command.creation_flags(0x0800_0000);
        Ok(command)
    }
}

/// `work`, unless it takes longer than `deadline` or `stop` resolves
/// first.
async fn bounded<T>(
    work: impl Future<Output = Result<T, RunError>>,
    deadline: Duration,
    stop: impl Future<Output = ()>,
) -> Result<T, RunError> {
    tokio::select! {
        done = tokio::time::timeout(deadline, work) => {
            done.map_err(|_| RunError::TimedOut(deadline))?
        }
        () = stop => Err(RunError::Stopped),
    }
}

/// The process groups a run's programs lead. Whatever a program starts
/// joins its group, and a confined one cannot leave it, so stopping the
/// group stops them all. Dropping it kills them.
struct Groups(Vec<u32>);

impl Groups {
    fn of<const N: usize>(children: [&tokio::process::Child; N]) -> Self {
        Self(children.iter().filter_map(|c| c.id()).collect())
    }

    fn kill(&self) {
        #[cfg(unix)]
        for &group in &self.0 {
            if let Some(group) = i32::try_from(group)
                .ok()
                .and_then(rustix::process::Pid::from_raw)
            {
                let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
            }
        }
    }

    /// Kills whatever the run left running, finished or stopped, and on
    /// Linux waits until none of it runs, so nothing it started still
    /// writes once the run returns.
    async fn end(mut self) {
        let mut pause = Duration::from_millis(1);
        loop {
            self.kill();
            if !self.0.iter().any(|&group| running(group)) {
                break;
            }
            tokio::time::sleep(pause).await;
            pause = (pause * 2).min(Duration::from_millis(100));
        }
        self.0.clear();
    }
}

impl Drop for Groups {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Whether a process of `group` runs and has not exited: its fields after
/// the command name in `/proc/<pid>/stat` are its state, parent, and group.
#[cfg(target_os = "linux")]
fn running(group: u32) -> bool {
    let Ok(processes) = std::fs::read_dir("/proc") else {
        return false;
    };
    let group = group.to_string();
    processes.flatten().any(|process| {
        let numbered = process.file_name().as_encoded_bytes()[..]
            .iter()
            .all(u8::is_ascii_digit);
        numbered
            && std::fs::read_to_string(process.path().join("stat")).is_ok_and(|stat| {
                let mut fields = stat
                    .rsplit_once(')')
                    .map_or("", |(_, f)| f)
                    .split_whitespace();
                let (state, group_of) = (fields.next(), fields.nth(1));
                !matches!(state, None | Some("Z" | "X" | "x")) && group_of == Some(group.as_str())
            })
    })
}

#[cfg(not(target_os = "linux"))]
fn running(_: u32) -> bool {
    false
}

/// Waits for `child` to exit.
async fn wait(child: &mut tokio::process::Child) -> Result<std::process::ExitStatus, RunError> {
    child
        .wait()
        .await
        .map_err(|e| RunError::Spawn(e.to_string()))
}

/// What a run that exited with `status` wrote, if it succeeded.
fn finished(
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
) -> Result<Output, RunError> {
    let stderr = String::from_utf8_lossy(&stderr).into_owned();
    if !status.success() {
        return Err(RunError::Failed {
            status: status.code(),
            stderr,
        });
    }
    Ok(Output { stdout, stderr })
}

/// A plugin as Settings changes it.
#[derive(Debug)]
pub struct Plugin {
    describe: Describe,
    check: Check,
    found: Arc<Program>,
    updatable: bool,
    active: RwLock<Active>,
}

/// What a plugin uses now.
#[derive(Debug, Clone)]
struct Active {
    installed: Option<Arc<Program>>,
    enabled: bool,
}

impl Plugin {
    /// The plugin, using `found` (at a configured path or on `PATH`) until
    /// a copy is installed; `updatable` if Wayfetch can install releases
    /// of it.
    pub fn new(describe: Describe, check: Check, found: Program, updatable: bool) -> Self {
        Self {
            describe,
            check,
            found: Arc::new(found),
            updatable,
            active: RwLock::new(Active {
                installed: None,
                enabled: true,
            }),
        }
    }

    fn active(&self) -> Active {
        self.active
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn update_active(&self, change: impl FnOnce(&mut Active)) {
        change(&mut self.active.write().unwrap_or_else(PoisonError::into_inner));
    }

    /// What it is.
    pub fn describe(&self) -> Describe {
        self.describe
    }

    /// How its program is checked.
    pub fn check(&self) -> &Check {
        &self.check
    }

    /// Whether Wayfetch can install releases of it.
    pub fn updatable(&self) -> bool {
        self.updatable
    }

    /// The copy in use: the installed one if any, else the one found.
    pub fn program(&self) -> Arc<Program> {
        self.active()
            .installed
            .unwrap_or_else(|| self.found.clone())
    }

    /// The copy found without Wayfetch installing one.
    pub fn found(&self) -> Arc<Program> {
        self.found.clone()
    }

    /// The copy Wayfetch installed, if one is in use.
    pub fn installed(&self) -> Option<Arc<Program>> {
        self.active().installed
    }

    /// Uses `installed` from now on, or the copy found if `None`.
    pub fn set_installed(&self, installed: Option<Program>) {
        self.update_active(|a| a.installed = installed.map(Arc::new));
    }

    /// Whether it is turned on.
    pub fn enabled(&self) -> bool {
        self.active().enabled
    }

    /// Turns it on or off.
    pub fn set_enabled(&self, enabled: bool) {
        self.update_active(|a| a.enabled = enabled);
    }

    /// Whether its services can use it: turned on and working.
    pub fn usable(&self) -> bool {
        let active = self.active();
        active.enabled && active.installed.as_deref().unwrap_or(&self.found).ready()
    }

    /// The plugin as Settings shows it, used by `services`.
    pub fn summary(&self, services: Vec<ProviderId>) -> PluginSummary {
        let active = self.active();
        let program = active.installed.as_deref().unwrap_or(&self.found);
        PluginSummary {
            id: self.describe.id.into(),
            name: self.describe.name.into(),
            role: self.describe.role,
            services,
            state: program.state,
            version: program.version.clone(),
            problem: program.problem.as_deref().map(DiagnosticText::new),
            enabled: active.enabled,
            updatable: self.updatable,
            installed: active.installed.is_some(),
            fallback_version: active.installed.as_ref().and(self.found.version.clone()),
        }
    }
}

/// Starts `command`. A program file just written, such as an update
/// Settings installed or a test's script, can be refused as busy while
/// another thread's child, started at that moment, still holds it open
/// for writing until that child execs; such a start is tried again.
async fn spawn(command: &mut tokio::process::Command) -> Result<tokio::process::Child, RunError> {
    /// Linux's `ETXTBSY`.
    const BUSY: i32 = 26;
    let mut tries = 0;
    loop {
        match command.spawn() {
            Err(e) if cfg!(target_os = "linux") && e.raw_os_error() == Some(BUSY) && tries < 10 => {
                tries += 1;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            started => return started.map_err(|e| RunError::Spawn(e.to_string())),
        }
    }
}

/// Reads `source` to its end, keeping at most `limit` bytes; more is an
/// error if `strict`, else discarded.
async fn read(
    mut source: impl AsyncRead + Unpin,
    limit: usize,
    strict: bool,
) -> Result<Vec<u8>, RunError> {
    let mut kept = Vec::new();
    let mut chunk = vec![0; 16 * 1024];
    loop {
        let n = source
            .read(&mut chunk)
            .await
            .map_err(|e| RunError::Spawn(e.to_string()))?;
        if n == 0 {
            return Ok(kept);
        }
        let room = limit - kept.len();
        if n > room && strict {
            return Err(RunError::TooMuchOutput(limit));
        }
        kept.extend_from_slice(&chunk[..n.min(room)]);
    }
}

/// Bounds of one run.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Longest it may take.
    pub deadline: Duration,
    /// The most standard output it may write.
    pub stdout_bytes: usize,
}

/// What a successful run wrote.
#[derive(Debug)]
pub struct Output {
    /// Its standard output.
    pub stdout: Vec<u8>,
    /// Its error output, at most 64 KiB.
    pub stderr: String,
}

/// Why a run failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RunError {
    /// The program was not found.
    #[error("the program was not found")]
    Missing,
    /// It could not be started or read.
    #[error("it could not be started: {0}")]
    Spawn(String),
    /// It must run confined, and this system cannot confine it.
    #[error("{0}, so it is not used")]
    Unconfined(String),
    /// It ran past its deadline and was stopped.
    #[error("it took longer than {0:?} and was stopped")]
    TimedOut(Duration),
    /// It was asked to stop, and stopped.
    #[error("it was stopped")]
    Stopped,
    /// It wrote more than its output limit and was stopped.
    #[error("it wrote more than {0} bytes and was stopped")]
    TooMuchOutput(usize),
    /// It exited with a failure.
    #[error("it failed{}: {}", status.map(|s| format!(" with status {s}")).unwrap_or_default(), last_line(stderr))]
    Failed {
        /// Its exit status, if it exited rather than being killed.
        status: Option<i32>,
        /// Its error output.
        stderr: String,
    },
}

/// The last line of `text` that says anything, at most 300 characters.
pub fn last_line(text: &str) -> String {
    let line = text
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("no message");
    line.chars().take(300).collect()
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    const DESCRIBE: Describe = Describe {
        id: "fake",
        name: "Fake",
        role: PluginRole::MediaResolver,
    };

    /// A shell script program in `dir`.
    pub(crate) fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn first_line(out: &str) -> Option<String> {
        out.lines().next().map(str::to_owned)
    }

    pub(crate) fn check() -> Check {
        Check {
            args: &["--version"],
            version: first_line,
            env: Vec::new(),
            confinement: None,
        }
    }

    #[tokio::test]
    async fn detection_reads_the_version_or_says_what_is_wrong() {
        let dir = tempfile::tempdir().unwrap();
        let ok = script(dir.path(), "ok", "echo 1.2.3");
        let program = Program::detect(DESCRIBE, Some(ok), &check()).await;
        let summary = Plugin::new(DESCRIBE, check(), program, false).summary(Vec::new());
        assert_eq!(summary.state, PluginState::Ready, "{:?}", summary.problem);
        assert_eq!(summary.version.as_deref(), Some("1.2.3"));

        let missing = Program::detect(DESCRIBE, None, &check()).await;
        assert_eq!(missing.state, PluginState::Missing);
        assert!(!missing.ready());

        let broken = script(dir.path(), "broken", "echo nope >&2; exit 3");
        let program = Program::detect(DESCRIBE, Some(broken), &check()).await;
        let summary = Plugin::new(DESCRIBE, check(), program, false).summary(Vec::new());
        assert_eq!(summary.state, PluginState::Failed);
        assert_eq!(
            summary.problem.as_ref().map(DiagnosticText::as_str),
            Some("Fake does not run: it failed with status 3: nope")
        );
    }

    #[tokio::test]
    async fn runs_are_bounded_in_time_and_output() {
        let dir = tempfile::tempdir().unwrap();
        let limits = Limits {
            deadline: Duration::from_millis(300),
            stdout_bytes: 1000,
        };
        let slow = Program {
            path: Some(script(dir.path(), "slow", "sleep 30")),
            ..Program::detect(DESCRIBE, None, &check()).await
        };
        let started = std::time::Instant::now();
        assert_eq!(
            slow.run(["x"], limits).await.unwrap_err(),
            RunError::TimedOut(limits.deadline)
        );
        assert!(started.elapsed() < Duration::from_secs(10));

        let loud = Program {
            path: Some(script(dir.path(), "loud", "yes")),
            ..slow.clone()
        };
        assert_eq!(
            loud.run(["x"], limits).await.unwrap_err(),
            RunError::TooMuchOutput(1000)
        );

        // Arguments are passed as they are, never through a shell.
        let echo = Program {
            path: Some(script(dir.path(), "echo", "printf '%s|' \"$@\"")),
            ..slow
        };
        let out = echo.run(["a b", "$HOME", ";x"], limits).await.unwrap();
        assert_eq!(out.stdout, b"a b|$HOME|;x|");

        // Its environment is the host's plus what it is given.
        let env = Program {
            path: Some(script(
                dir.path(),
                "env",
                "printf '%s' \"$WAYFETCH_TEST_PLUGIN_DIR\"",
            )),
            ..echo
        }
        .with_env("WAYFETCH_TEST_PLUGIN_DIR", "/x/y");
        assert_eq!(env.run(["x"], limits).await.unwrap().stdout, b"/x/y");
    }

    #[tokio::test]
    async fn a_plugin_uses_its_installed_copy_until_it_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let found = script(dir.path(), "found", "echo 1.0");
        let found = Program::detect(DESCRIBE, Some(found), &check()).await;
        let plugin = Plugin::new(DESCRIBE, check(), found, true);
        let summary = plugin.summary(Vec::new());
        assert!(summary.enabled && summary.updatable && !summary.installed);
        assert_eq!(summary.fallback_version, None);

        let newer = script(dir.path(), "newer", "echo 2.0");
        plugin.set_installed(Some(Program::detect(DESCRIBE, Some(newer), &check()).await));
        // A run that already took the program keeps it.
        let running = plugin.program();
        let summary = plugin.summary(Vec::new());
        assert_eq!(summary.version.as_deref(), Some("2.0"));
        assert!(summary.installed);
        assert_eq!(summary.fallback_version.as_deref(), Some("1.0"));

        plugin.set_installed(None);
        plugin.set_enabled(false);
        assert_eq!(running.version(), Some("2.0"));
        assert_eq!(plugin.program().version(), Some("1.0"));
        let summary = plugin.summary(Vec::new());
        assert!(!summary.enabled && !summary.usable());
        assert!(!plugin.usable());
    }
}
