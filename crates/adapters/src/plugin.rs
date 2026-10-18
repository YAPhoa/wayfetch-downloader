//! Helper programs integrations run, such as yt-dlp and ffmpeg (ADR-27).
//!
//! A program is found at a path the host configures, or on `PATH`, and
//! asked for its version once when the host starts: that answer is what
//! Settings > Plugins shows. Each run is a separate process started
//! without a shell, with no standard input, with its output bounded and a
//! deadline; dropping a run stops the process. A helper the program
//! starts itself is not tracked.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
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

/// A helper program and what its version check found.
#[derive(Debug, Clone)]
pub struct Program {
    describe: Describe,
    path: Option<PathBuf>,
    state: PluginState,
    version: Option<String>,
    problem: Option<String>,
    env: Vec<(&'static str, OsString)>,
}

impl Program {
    /// Finds the program at `path` and checks it runs by asking it for its
    /// version with `args`; `version` reads the version from the answer.
    /// No path means the program is missing.
    pub async fn detect(
        describe: Describe,
        path: Option<PathBuf>,
        args: &[&str],
        version: fn(&str) -> Option<String>,
    ) -> Self {
        let mut program = Self {
            describe,
            path,
            state: PluginState::Missing,
            version: None,
            problem: None,
            env: Vec::new(),
        };
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
        match program.run(args, limits).await {
            Ok(output) => {
                program.state = PluginState::Ready;
                program.version = version(&String::from_utf8_lossy(&output.stdout));
            }
            Err(e) => {
                program.state = PluginState::Failed;
                program.problem = Some(format!("{} does not run: {e}", describe.name));
            }
        }
        program
    }

    /// Whether it can be used.
    pub fn ready(&self) -> bool {
        self.state == PluginState::Ready
    }

    /// The plugin as Settings shows it, used by `services`.
    pub fn summary(&self, services: Vec<ProviderId>) -> PluginSummary {
        PluginSummary {
            id: self.describe.id.into(),
            name: self.describe.name.into(),
            role: self.describe.role,
            services,
            state: self.state,
            version: self.version.clone(),
            problem: self.problem.as_deref().map(DiagnosticText::new),
        }
    }

    /// The program run with `key` set to `value` in its environment,
    /// unless the host's own environment sets `key`.
    pub fn with_env(mut self, key: &'static str, value: impl Into<OsString>) -> Self {
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
        let path = self.path.as_ref().ok_or(RunError::Missing)?;
        let mut command = tokio::process::Command::new(path);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            // A Python program writes UTF-8 whatever the host's locale.
            .env("PYTHONIOENCODING", "utf-8")
            .envs(self.env.iter().map(|(k, v)| (k, v)));
        #[cfg(windows)]
        // No console window opens for it on the desktop.
        command.creation_flags(0x0800_0000);
        let mut child = command
            .spawn()
            .map_err(|e| RunError::Spawn(e.to_string()))?;
        let (stdout, stderr) = (
            child.stdout.take().ok_or(RunError::Missing)?,
            child.stderr.take().ok_or(RunError::Missing)?,
        );
        let ran = async {
            tokio::try_join!(
                read(stdout, limits.stdout_bytes, true),
                read(stderr, STDERR_BYTES, false),
                async {
                    child
                        .wait()
                        .await
                        .map_err(|e| RunError::Spawn(e.to_string()))
                },
            )
        };
        // On a timeout or an error, dropping the child stops it.
        let (stdout, stderr, status) = tokio::time::timeout(limits.deadline, ran)
            .await
            .map_err(|_| RunError::TimedOut(limits.deadline))??;
        let stderr = String::from_utf8_lossy(&stderr).into_owned();
        if !status.success() {
            return Err(RunError::Failed {
                status: status.code(),
                stderr,
            });
        }
        Ok(Output { stdout, stderr })
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
    /// It ran past its deadline and was stopped.
    #[error("it took longer than {0:?} and was stopped")]
    TimedOut(Duration),
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

    #[tokio::test]
    async fn detection_reads_the_version_or_says_what_is_wrong() {
        let dir = tempfile::tempdir().unwrap();
        let ok = script(dir.path(), "ok", "echo 1.2.3");
        let program = Program::detect(DESCRIBE, Some(ok), &["--version"], first_line).await;
        let summary = program.summary(Vec::new());
        assert_eq!(summary.state, PluginState::Ready);
        assert_eq!(summary.version.as_deref(), Some("1.2.3"));

        let missing = Program::detect(DESCRIBE, None, &[], first_line).await;
        assert_eq!(missing.summary(Vec::new()).state, PluginState::Missing);
        assert!(!missing.ready());

        let broken = script(dir.path(), "broken", "echo nope >&2; exit 3");
        let program = Program::detect(DESCRIBE, Some(broken), &[], first_line).await;
        let summary = program.summary(Vec::new());
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
            ..Program::detect(DESCRIBE, None, &[], first_line).await
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
}
