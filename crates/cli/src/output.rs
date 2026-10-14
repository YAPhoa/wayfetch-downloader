//! What the session prints, as text for people or as one JSON object per
//! line for scripts.
//!
//! Output is built only from the engine's client-visible types, which carry
//! no source URLs, so a signed URL or token never reaches routine output.
//! Input that was rejected is never echoed back either.

use std::cell::Cell;
use std::fmt::Write as _;
use std::io::{self, Write};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::time::Duration;

use serde::Serialize;
use wayfetch_engine::ShutdownReport;
use wayfetch_engine::domain::id::{BatchId, JobId};
use wayfetch_engine::domain::version::Revision;
use wayfetch_engine::protocol::event::{DurableEvent, EventKind, ProgressSample, ResyncReason};
use wayfetch_engine::protocol::query::JobSummary;
use wayfetch_engine::protocol::{InstanceId, ProtocolError};

/// Commands the session accepts, for `help`.
pub const HELP: &str = "\
commands:
  URL...          download one or more http(s) URLs (same as `add URL...`)
  list            list all jobs
  inspect JOB     show one job, for example `inspect job-1` or `inspect 1`
  pause JOB       pause a job, keeping its partial data
  resume JOB      resume a paused or cancelled job
  cancel JOB      stop a job, keeping its partial data
  help            show this help
  quit            shut down; unfinished jobs continue on the next start";

/// Something to print.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Output<'a> {
    /// The engine is running and the session reads commands.
    Ready {
        /// The engine instance.
        instance: InstanceId,
    },
    /// The download root cannot be used yet.
    RootUnavailable {
        /// Why.
        reason: String,
    },
    /// A durable change.
    Event {
        /// The change.
        event: &'a DurableEvent,
    },
    /// Live transfer progress.
    Progress(&'a ProgressSample),
    /// Events were missed; `list` shows the current state.
    Resync {
        /// Why.
        reason: ResyncReason,
    },
    /// Inputs were accepted.
    Submitted {
        /// The batch.
        batch: BatchId,
        /// Inputs accepted.
        accepted: u32,
    },
    /// A job, answering `inspect` or as one row of `list`.
    Job {
        /// The job.
        job: &'a JobSummary,
    },
    /// The end of a `list`.
    Listed {
        /// Jobs listed.
        count: u64,
    },
    /// A pause, resume, or cancel was applied.
    Updated {
        /// The job.
        job: JobId,
        /// Its revision afterwards.
        revision: Revision,
        /// Whether anything changed.
        changed: bool,
    },
    /// The command list.
    Help {
        /// The text.
        text: &'static str,
    },
    /// A line or command was rejected; the session continues.
    Error {
        /// Stable code: an engine error code, or one of the session's own
        /// (`line_too_long`, `not_utf8`, `invalid_input`).
        code: String,
        /// What went wrong. Never contains the rejected input.
        message: String,
    },
    /// Shutdown started.
    ShuttingDown {
        /// Why.
        reason: &'static str,
    },
    /// Output was dropped because nothing read it fast enough.
    Dropped {
        /// Lines dropped.
        count: u64,
    },
    /// The engine stopped.
    Stopped {
        /// Whether every engine task finished in time.
        clean: bool,
        /// Engine tasks still running after the deadline.
        unfinished_tasks: usize,
        /// Whether the state directory was released.
        state_dir_released: bool,
    },
}

impl Output<'_> {
    /// An error the session itself detected.
    pub fn error(code: &str, message: &str) -> Self {
        Self::Error {
            code: code.into(),
            message: message.into(),
        }
    }

    /// The summary of a shutdown.
    pub fn stopped(report: &ShutdownReport) -> Self {
        Self::Stopped {
            clean: report.clean,
            unfinished_tasks: report.unfinished_tasks,
            state_dir_released: report.state_dir_released,
        }
    }

    /// The text form, or `None` for output only scripts need.
    fn text(&self) -> Option<String> {
        Some(match self {
            Self::Ready { instance } => format!(
                "wayfetch ready (instance {instance}). Type a URL to download it, or `help`."
            ),
            Self::RootUnavailable { reason } => format!(
                "warning: the download root cannot be used ({reason}); downloads wait until it \
                 can. Initialize a new root with `wayfetch init-root`."
            ),
            Self::Event { event } => match &event.kind {
                EventKind::BatchAccepted { .. } => return None,
                EventKind::JobAdded { job } => format!("added {}", job_line(job)),
                EventKind::JobChanged { job } => job_line(job),
            },
            Self::Progress(p) => format!(
                "{} {} at {} B/s",
                p.job,
                bytes(p.bytes_received, p.bytes_total),
                p.bytes_per_second
            ),
            Self::Resync { reason } => {
                format!("missed events ({reason}); `list` shows the current state")
            }
            Self::Submitted { batch, accepted } => format!("{batch}: accepted {accepted} URL(s)"),
            Self::Job { job } => job_line(job),
            Self::Listed { count } => format!("{count} job(s)"),
            Self::Updated { job, changed, .. } => {
                format!(
                    "{job}: {}",
                    if *changed { "requested" } else { "no change" }
                )
            }
            Self::Help { text } => (*text).to_owned(),
            Self::Error { message, .. } => format!("error: {message}"),
            Self::Dropped { count } => {
                format!("{count} line(s) of output dropped: the reader fell behind")
            }
            Self::ShuttingDown { reason } => format!("shutting down ({reason})"),
            Self::Stopped {
                clean: true,
                state_dir_released: true,
                ..
            } => "stopped; unfinished jobs continue on the next start".to_owned(),
            Self::Stopped {
                unfinished_tasks, ..
            } => format!(
                "stopped uncleanly: {unfinished_tasks} engine task(s) still running; the state \
                 directory stays locked until this process exits"
            ),
        })
    }

    fn is_problem(&self) -> bool {
        matches!(
            self,
            Self::Error { .. }
                | Self::RootUnavailable { .. }
                | Self::Resync { .. }
                | Self::Dropped { .. }
        )
    }
}

impl From<ProtocolError> for Output<'_> {
    fn from(error: ProtocolError) -> Self {
        Self::Error {
            code: error.code.to_string(),
            message: error.message.to_string(),
        }
    }
}

/// One job on one line: ID, state, bytes, and outcome.
fn job_line(job: &JobSummary) -> String {
    let mut line = format!(
        "{} {} {}",
        job.id,
        job.display,
        bytes(job.bytes_done, job.bytes_total)
    );
    if let Some(failure) = &job.failure {
        let _ = write!(line, " [{}]", failure.code);
        if let Some(message) = &failure.message {
            let _ = write!(line, " {message}");
        }
    }
    if let Some(artifact) = &job.artifact {
        let _ = write!(line, " -> {}", artifact.path);
    }
    line
}

fn bytes(done: u64, total: Option<u64>) -> String {
    match total {
        Some(total) if total > 0 => format!(
            "{done}/{total} bytes ({}%)",
            u128::from(done) * 100 / u128::from(total)
        ),
        Some(total) => format!("{done}/{total} bytes"),
        None => format!("{done} bytes"),
    }
}

/// Output could not be written: standard output is closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Closed;

#[derive(Debug, Clone, Copy)]
enum Stream {
    Out,
    Err,
}

/// The process's standard output and error, written by a dedicated thread
/// through a bounded queue.
///
/// Nothing that prints ever waits for the terminal: a reader that stops
/// reading fills the queue, and further lines are dropped and counted
/// instead of blocking the session, its signal handling, or shutdown. A
/// write blocked in the thread holds up neither the runtime nor process
/// exit.
#[derive(Debug)]
pub struct Terminal {
    queue: SyncSender<(Stream, String)>,
    /// Disconnects when the writer thread ends.
    done: Receiver<()>,
    /// Lines dropped since the last one queued.
    dropped: Cell<u64>,
}

/// Lines queued for the terminal at once.
const QUEUE_LINES: usize = 256;
/// How long [`Terminal::finish`] lets queued lines drain before exiting.
pub const DRAIN: Duration = Duration::from_secs(1);

impl Terminal {
    /// Starts the writer thread.
    pub fn start() -> Self {
        let (queue, lines) = mpsc::sync_channel(QUEUE_LINES);
        let (alive, done) = mpsc::sync_channel(0);
        std::thread::Builder::new()
            .name("wayfetch-output".into())
            .spawn(move || {
                let _alive = alive;
                write_lines(&lines);
            })
            .expect("spawn the output thread");
        Self {
            queue,
            done,
            dropped: Cell::new(0),
        }
    }

    /// Queues `text` as a line on `stream`. Returns whether it was queued
    /// rather than dropped because the queue is full.
    fn try_write(&self, stream: Stream, text: String) -> Result<bool, Closed> {
        match self.queue.try_send((stream, text)) {
            Ok(()) => Ok(true),
            Err(TrySendError::Full(_)) => Ok(false),
            Err(TrySendError::Disconnected(_)) => Err(Closed),
        }
    }

    /// Reports an error that ends the program.
    pub fn fatal(&self, message: &str) {
        let _ = self.try_write(Stream::Err, format!("error: {message}"));
    }

    /// Prints a plain line on standard output.
    pub fn line(&self, text: String) {
        let _ = self.try_write(Stream::Out, text);
    }

    /// Lets queued lines drain for at most `wait`, then gives up on a
    /// reader that stopped reading.
    pub fn finish(self, wait: Duration) {
        drop(self.queue);
        let _ = self.done.recv_timeout(wait);
    }
}

/// Writes queued lines until the queue closes or standard output fails.
fn write_lines(lines: &Receiver<(Stream, String)>) {
    for (stream, text) in lines {
        match stream {
            Stream::Out => {
                let mut out = io::stdout().lock();
                if writeln!(out, "{text}").and_then(|()| out.flush()).is_err() {
                    return;
                }
            }
            // Nothing useful is left to do if standard error is gone.
            Stream::Err => {
                let _ = writeln!(io::stderr(), "{text}");
            }
        }
    }
}

/// Prints session output to a [`Terminal`].
#[derive(Debug, Clone, Copy)]
pub struct Printer<'a> {
    json: bool,
    terminal: &'a Terminal,
}

impl<'a> Printer<'a> {
    /// Text, or JSON lines if `json`.
    pub fn new(json: bool, terminal: &'a Terminal) -> Self {
        Self { json, terminal }
    }

    /// Queues `output` without waiting. In text mode, problems go to
    /// standard error. Output that does not fit is dropped and reported
    /// once there is room again. Fails only once standard output is closed.
    pub fn print(&self, output: &Output<'_>) -> Result<(), Closed> {
        let dropped = &self.terminal.dropped;
        if dropped.get() > 0 {
            if !self.write(&Output::Dropped {
                count: dropped.get(),
            })? {
                dropped.set(dropped.get() + 1);
                return Ok(());
            }
            dropped.set(0);
        }
        if !self.write(output)? {
            dropped.set(dropped.get() + 1);
        }
        Ok(())
    }

    /// Queues `output`; returns whether it fit.
    fn write(&self, output: &Output<'_>) -> Result<bool, Closed> {
        if self.json {
            let line = serde_json::to_string(output).expect("output serializes");
            return self.terminal.try_write(Stream::Out, line);
        }
        let Some(text) = output.text() else {
            return Ok(true);
        };
        let stream = if output.is_problem() {
            Stream::Err
        } else {
            Stream::Out
        };
        self.terminal.try_write(stream, text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_queue_drops_output_and_reports_it_when_there_is_room() {
        let (queue, lines) = mpsc::sync_channel(2);
        let (_alive, done) = mpsc::sync_channel(0);
        let terminal = Terminal {
            queue,
            done,
            dropped: Cell::new(0),
        };
        let printer = Printer::new(true, &terminal);
        let listed = |count| Output::Listed { count };
        for count in 0..5 {
            assert_eq!(printer.print(&listed(count)), Ok(()));
        }
        let read = || -> Vec<String> { lines.try_iter().map(|(_, text)| text).collect() };
        assert_eq!(read().len(), 2);
        printer.print(&listed(9)).unwrap();
        assert_eq!(
            read(),
            [
                r#"{"type":"dropped","count":3}"#,
                r#"{"type":"listed","count":9}"#
            ]
        );
        drop(lines);
        assert_eq!(printer.print(&listed(1)), Err(Closed));
    }

    #[test]
    fn byte_counts_show_a_percentage_when_the_total_is_known() {
        assert_eq!(bytes(5, None), "5 bytes");
        assert_eq!(bytes(0, Some(0)), "0/0 bytes");
        assert_eq!(bytes(1, Some(3)), "1/3 bytes (33%)");
        assert_eq!(
            bytes(u64::MAX, Some(u64::MAX)),
            format!("{0}/{0} bytes (100%)", u64::MAX)
        );
    }
}
