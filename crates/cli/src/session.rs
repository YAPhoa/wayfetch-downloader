//! The foreground session: starts the engine, turns input lines into engine
//! commands and queries, prints durable events as they happen, and shuts
//! the engine down gracefully on `quit`, end of input, or a signal.
//!
//! Shutdown never cancels jobs: the engine keeps every unfinished job
//! durable and takes it over on the next start. Job control goes through
//! the same commands every host uses, each with a fresh idempotency key and
//! the revision the session just read, so a job that changed in between is
//! reported as a conflict rather than acted on blindly.

use std::convert::Infallible;
use std::process::ExitCode;

use wayfetch_engine::domain::id::JobId;
use wayfetch_engine::domain::version::Revision;
use wayfetch_engine::protocol::command::{CommandResult, InputUrl, SubmitItems};
use wayfetch_engine::protocol::event::Subscribe;
use wayfetch_engine::protocol::query::{JobFilter, JobSummary, PageSize};
use wayfetch_engine::protocol::{
    Command, CommandEnvelope, ErrorCode, IdempotencyKey, Permission, ProtocolError, Query,
    QueryResult, StreamItem,
};
use wayfetch_engine::{EngineHandle, ShutdownRequest, Subscription};
use wayfetch_host::{NoServer, Printer, Signals, Started, Terminal};

use crate::RunArgs;
use crate::input::{self, Line};
use crate::output::{HELP, Output};

/// The local operator may do everything.
const OPERATOR: &[Permission] = Permission::ALL;

/// Runs a session until it ends; the exit code says whether the engine
/// stopped cleanly.
pub async fn run(args: RunArgs, terminal: &Terminal) -> Result<ExitCode, String> {
    let printer = Printer::new(args.json, terminal);
    // Registered before the engine starts, so a signal during startup
    // still leads to a graceful shutdown.
    let mut signals = Signals::new()?;
    let (settings, _) = args.engine.load::<NoServer>()?;
    let started = wayfetch_host::start_engine(settings).await?;
    let root_problem = started.root_problem();
    let Started { owner, .. } = started;
    let session = Session {
        engine: owner.handle(),
        printer,
    };

    let reason = match session.prepare(root_problem).await {
        Ok(subscription) => {
            session
                .serve(subscription, input::spawn(), &mut signals)
                .await
        }
        Err(Stop(reason)) => reason,
    };
    let _ = printer.print(&Output::ShuttingDown { reason });
    let report = owner.shutdown(ShutdownRequest::default()).await;
    let _ = printer.print(&Output::stopped(&report));
    Ok(if report.clean {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Why the session ended.
struct Stop(&'static str);

/// What to do after a line.
enum Next {
    Continue,
    Quit,
}

struct Session<'a> {
    engine: EngineHandle,
    printer: Printer<'a>,
}

impl Session<'_> {
    /// Announces the session and subscribes to events.
    async fn prepare(&self, root_problem: Option<String>) -> Result<Subscription, Stop> {
        let instance = self.engine.instance();
        self.print(&Output::Ready { instance })?;
        if let Some(reason) = root_problem {
            self.print(&Output::RootUnavailable { reason })?;
        }
        self.subscribe().await
    }

    /// Reads commands and prints events until the session ends.
    async fn serve(
        &self,
        mut events: Subscription,
        mut lines: tokio::sync::mpsc::Receiver<Line>,
        signals: &mut Signals,
    ) -> &'static str {
        let ended: Result<Infallible, Stop> = async {
            loop {
                tokio::select! {
                    // Input first, so `quit` wins over a stream of events.
                    biased;
                    () = signals.recv() => return Err(Stop("interrupted")),
                    () = self.printer.closed() => return Err(Stop("output closed")),
                    line = lines.recv() => match line {
                        None => return Err(Stop("end of input")),
                        Some(line) => {
                            if let Next::Quit = self.line(line).await? {
                                return Err(Stop("quit"));
                            }
                        }
                    },
                    item = events.next() => match item {
                        Some(item) => {
                            if let Some(next) = self.event(item).await? {
                                events = next;
                            }
                        }
                        None => return Err(Stop("engine stopped")),
                    },
                }
            }
        }
        .await;
        match ended {
            Err(Stop(reason)) => reason,
        }
    }

    /// Prints a stream item; returns a new subscription after a resync.
    async fn event(&self, item: StreamItem) -> Result<Option<Subscription>, Stop> {
        match item {
            StreamItem::Hello(_) => {}
            StreamItem::Event(event) => self.print(&Output::Event { event: &event })?,
            StreamItem::Progress(sample) => self.print(&Output::Progress(&sample))?,
            StreamItem::Resync { reason } => {
                self.print(&Output::Resync { reason })?;
                return self.subscribe().await.map(Some);
            }
        }
        Ok(None)
    }

    async fn subscribe(&self) -> Result<Subscription, Stop> {
        self.engine
            .subscribe(Subscribe { after: None }, OPERATOR)
            .await
            .map_err(|_| Stop("engine stopped"))
    }

    /// Handles one input line.
    async fn line(&self, line: Line) -> Result<Next, Stop> {
        let text = match line {
            Line::Text(text) => text,
            Line::TooLong => {
                self.print(&Output::error(
                    "line_too_long",
                    &format!("line longer than {} bytes ignored", input::MAX_LINE_BYTES),
                ))?;
                return Ok(Next::Continue);
            }
            Line::NotUtf8 => {
                self.print(&Output::error("not_utf8", "line is not UTF-8; ignored"))?;
                return Ok(Next::Continue);
            }
        };
        let mut words = text.split_ascii_whitespace();
        let Some(first) = words.next() else {
            return Ok(Next::Continue);
        };
        let result = match first {
            "quit" | "exit" => return Ok(Next::Quit),
            "help" => self.print(&Output::Help { text: HELP }),
            "list" => self.list().await,
            "add" => self.submit(words).await,
            "inspect" => match one_job(first, words) {
                Ok(job) => match self.inspect(job).await {
                    Ok(summary) => self.print(&Output::Job { job: &summary }),
                    Err(e) => self.print(&e.into()),
                },
                Err(usage) => self.print(&usage),
            },
            "pause" => {
                self.control(first, words, |job, expected_revision| Command::Pause {
                    job,
                    expected_revision,
                })
                .await
            }
            "resume" => {
                self.control(first, words, |job, expected_revision| Command::Resume {
                    job,
                    expected_revision,
                })
                .await
            }
            "cancel" => {
                self.control(first, words, |job, expected_revision| Command::Cancel {
                    job,
                    expected_revision,
                })
                .await
            }
            "retry" => {
                self.control(first, words, |job, expected_revision| Command::Retry {
                    job,
                    expected_revision,
                    repeat_provider_operation: false,
                })
                .await
            }
            _ => self.submit(text.split_ascii_whitespace()).await,
        };
        result.map(|()| Next::Continue)
    }

    /// Pauses, resumes, cancels, or retries the job named in `words`, at
    /// the revision just read.
    async fn control<'a>(
        &self,
        verb: &str,
        words: impl Iterator<Item = &'a str>,
        command: fn(JobId, Revision) -> Command,
    ) -> Result<(), Stop> {
        let job = match one_job(verb, words) {
            Ok(job) => job,
            Err(usage) => return self.print(&usage),
        };
        let result = match self.inspect(job).await {
            Ok(summary) => self.execute(command(job, summary.revision)).await,
            Err(e) => Err(e),
        };
        self.print(&match result {
            Ok(CommandResult::JobUpdated {
                job,
                revision,
                changed,
            }) => Output::Updated {
                job,
                revision,
                changed,
            },
            Ok(
                CommandResult::Submitted { .. }
                | CommandResult::SettingsUpdated { .. }
                | CommandResult::LinksCollected { .. }
                | CommandResult::LinksApplied { .. }
                | CommandResult::AuthorizationStarted(_)
                | CommandResult::AccountAuthorized { .. }
                | CommandResult::AccountUpdated { .. },
            ) => unexpected().into(),
            Err(e) => e.into(),
        })
    }

    /// Submits the URLs in `words` as one batch. A rejected URL is
    /// reported by position only.
    async fn submit<'a>(&self, words: impl Iterator<Item = &'a str>) -> Result<(), Stop> {
        let mut items = Vec::new();
        for (i, word) in words.enumerate() {
            match InputUrl::parse(word) {
                Ok(url) => items.push(url),
                Err(e) => {
                    let what = if i == 0 && !word.contains(':') {
                        "unknown command; type `help`".to_owned()
                    } else {
                        format!("URL {} rejected: {}", i + 1, e.message)
                    };
                    return self.print(&Output::Error {
                        code: e.code.to_string(),
                        message: what,
                    });
                }
            }
        }
        let result = match SubmitItems::try_from(items) {
            Ok(items) => {
                self.execute(Command::Submit {
                    items,
                    package: None,
                })
                .await
            }
            Err(e) => Err(e),
        };
        self.print(&match result {
            Ok(CommandResult::Submitted { batch, accepted }) => {
                Output::Submitted { batch, accepted }
            }
            Ok(
                CommandResult::JobUpdated { .. }
                | CommandResult::SettingsUpdated { .. }
                | CommandResult::LinksCollected { .. }
                | CommandResult::LinksApplied { .. }
                | CommandResult::AuthorizationStarted(_)
                | CommandResult::AccountAuthorized { .. }
                | CommandResult::AccountUpdated { .. },
            ) => unexpected().into(),
            Err(e) => e.into(),
        })
    }

    /// Prints every job, a bounded page at a time.
    async fn list(&self) -> Result<(), Stop> {
        let mut after = None;
        let mut count = 0;
        loop {
            let query = Query::ListJobs {
                filter: JobFilter::default(),
                after,
                page_size: PageSize::DEFAULT,
            };
            let page = match self.engine.query(query, OPERATOR).await {
                Ok(QueryResult::Jobs(page)) => page,
                Ok(_) => return self.print(&unexpected().into()),
                Err(e) => return self.print(&e.into()),
            };
            for job in &page.items {
                self.print(&Output::Job { job })?;
            }
            count += page.items.len() as u64;
            match page.next_after {
                Some(next) => after = Some(next),
                None => return self.print(&Output::Listed { count }),
            }
        }
    }

    async fn inspect(&self, job: JobId) -> Result<JobSummary, ProtocolError> {
        match self
            .engine
            .query(Query::InspectJob { job }, OPERATOR)
            .await?
        {
            QueryResult::Job { job, .. } => Ok(job),
            _ => Err(unexpected()),
        }
    }

    async fn execute(&self, command: Command) -> Result<CommandResult, ProtocolError> {
        let envelope = CommandEnvelope {
            idempotency_key: IdempotencyKey::generate(),
            target: self.engine.instance(),
            command,
        };
        self.engine.execute(envelope, OPERATOR).await
    }

    /// Prints `output`; a closed output ends the session.
    fn print(&self, output: &Output<'_>) -> Result<(), Stop> {
        self.printer
            .print(output)
            .map_err(|_| Stop("output closed"))
    }
}

/// The single job argument of `verb`, or its usage.
fn one_job<'a>(
    verb: &str,
    mut words: impl Iterator<Item = &'a str>,
) -> Result<JobId, Output<'static>> {
    match (words.next().and_then(parse_job), words.next()) {
        (Some(job), None) => Ok(job),
        _ => Err(Output::error(
            "invalid_input",
            &format!("usage: {verb} JOB, for example `{verb} job-1`"),
        )),
    }
}

/// The engine answered with a result of the wrong kind.
fn unexpected() -> ProtocolError {
    ProtocolError::new(ErrorCode::Internal, "unexpected engine result")
}

/// `job-3` or `3`.
fn parse_job(text: &str) -> Option<JobId> {
    if text.bytes().all(|b| b.is_ascii_digit()) {
        format!("job-{text}").parse().ok()
    } else {
        text.parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jobs_are_named_by_id_or_number() {
        let one = JobId::from_u64(1);
        assert_eq!(parse_job("job-1"), one);
        assert_eq!(parse_job("1"), one);
        for bad in ["", "0", "01", "job-0", "-1", "job-x", "batch-1", "1e3"] {
            assert_eq!(parse_job(bad), None, "{bad}");
        }
    }
}
