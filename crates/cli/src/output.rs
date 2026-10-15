//! What the session prints, as text for people or as one JSON object per
//! line for scripts.
//!
//! Output is built only from the engine's client-visible types, which carry
//! no source URLs, so a signed URL or token never reaches routine output.
//! Input that was rejected is never echoed back either.

use std::fmt::Write as _;

use serde::Serialize;
use wayfetch_engine::ShutdownReport;
use wayfetch_engine::domain::id::{BatchId, JobId};
use wayfetch_engine::domain::version::Revision;
use wayfetch_engine::protocol::event::{DurableEvent, EventKind, ProgressSample, ResyncReason};
use wayfetch_engine::protocol::query::JobSummary;
use wayfetch_engine::protocol::{InstanceId, ProtocolError};
use wayfetch_host::Message;

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
}

impl Message for Output<'_> {
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
                EventKind::JobRemoved { job } => format!("{job}: removed from the list"),
                EventKind::SettingsChanged { .. } => "settings changed".to_owned(),
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

    fn dropped(count: u64) -> Self {
        Self::Dropped { count }
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

#[cfg(test)]
mod tests {
    use super::*;

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
