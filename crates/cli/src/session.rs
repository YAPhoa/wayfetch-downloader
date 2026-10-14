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
use std::sync::Arc;

use wayfetch_engine::domain::id::JobId;
use wayfetch_engine::domain::version::Revision;
use wayfetch_engine::fs::Root;
use wayfetch_engine::protocol::command::{CommandResult, InputUrl, SubmitItems};
use wayfetch_engine::protocol::event::Subscribe;
use wayfetch_engine::protocol::query::{JobFilter, JobSummary, PageSize};
use wayfetch_engine::protocol::{
    Command, CommandEnvelope, ErrorCode, IdempotencyKey, Permission, ProtocolError, Query,
    QueryResult, StreamItem,
};
use wayfetch_engine::{
    EngineBuilder, EngineConfig, EngineHandle, RootConfig, ShutdownRequest, Subscription,
};
use wayfetch_net::{CertificateDer, HttpConfig, HttpTransport};

use crate::input::{self, Line};
use crate::output::{HELP, Output, Printer, Terminal};
use crate::{DOWNLOAD_ROOT, RunArgs, absolute};

/// The local operator may do everything.
const OPERATOR: &[Permission] = &[Permission::Read, Permission::Submit, Permission::Control];

/// Largest CA certificate file accepted.
const MAX_CA_FILE_BYTES: u64 = 1024 * 1024;

/// Runs a session until it ends; the exit code says whether the engine
/// stopped cleanly.
pub async fn run(args: RunArgs, terminal: &Terminal) -> Result<ExitCode, String> {
    let printer = Printer::new(args.json, terminal);
    // Registered before the engine starts, so a signal during startup
    // still leads to a graceful shutdown.
    let mut signals = Signals::new()?;
    let state_dir = absolute(&args.state.state_dir)?;
    let root = absolute(&args.download_root)?;
    let transport = HttpTransport::new(http_config(&args)?)
        .map_err(|e| format!("cannot configure HTTP: {e}"))?;
    let mut config = EngineConfig::new(state_dir);
    config.roots.push(RootConfig {
        id: DOWNLOAD_ROOT,
        path: root.clone(),
    });
    config.download_root = Some(DOWNLOAD_ROOT);
    let owner = EngineBuilder::new(config, tokio::runtime::Handle::current())
        .transport(Arc::new(transport))
        .start()
        .await
        .map_err(|e| e.to_string())?;
    let session = Session {
        engine: owner.handle(),
        printer,
    };

    let reason = match session.prepare(&root).await {
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

/// Transport settings: the production defaults, plus only the exceptions
/// the operator named.
fn http_config(args: &RunArgs) -> Result<HttpConfig, String> {
    let mut config = HttpConfig::default();
    config
        .egress
        .allowed_networks
        .extend(args.allow_network.iter().copied());
    for path in &args.ca_cert {
        config.extra_roots.extend(read_ca(path)?);
    }
    Ok(config)
}

/// The certificates in a PEM file, or the single certificate in a DER file.
fn read_ca(path: &std::path::Path) -> Result<Vec<CertificateDer<'static>>, String> {
    use rustls_pki_types::pem::PemObject;
    use std::io::Read;

    let fail = |e: &dyn std::fmt::Display| format!("CA certificate {}: {e}", path.display());
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(MAX_CA_FILE_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|e| fail(&e))?;
    if bytes.len() as u64 > MAX_CA_FILE_BYTES {
        return Err(fail(&"file too large"));
    }
    if !bytes.starts_with(b"-----BEGIN") {
        return Ok(vec![CertificateDer::from(bytes)]);
    }
    let certs = CertificateDer::pem_slice_iter(&bytes)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| fail(&e))?;
    if certs.is_empty() {
        return Err(fail(&"no certificate found"));
    }
    Ok(certs)
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
    async fn prepare(&self, root: &std::path::Path) -> Result<Subscription, Stop> {
        let instance = self.engine.instance();
        self.print(&Output::Ready { instance })?;
        // The engine blocks jobs on an unusable root by itself; this only
        // tells the operator early, and never initializes anything.
        if let Err(e) = Root::open(root, DOWNLOAD_ROOT, instance) {
            self.print(&Output::RootUnavailable {
                reason: e.to_string(),
            })?;
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
            _ => self.submit(text.split_ascii_whitespace()).await,
        };
        result.map(|()| Next::Continue)
    }

    /// Pauses, resumes, or cancels the job named in `words`, at the
    /// revision just read.
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
            Ok(CommandResult::Submitted { .. }) => unexpected().into(),
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
            Ok(CommandResult::JobUpdated { .. }) => unexpected().into(),
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
                Ok(QueryResult::Job { .. }) => return self.print(&unexpected().into()),
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
            QueryResult::Jobs(_) => Err(unexpected()),
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

/// Requests to stop: Ctrl+C, and on Unix also `SIGTERM` from a service
/// manager.
struct Signals {
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
}

impl Signals {
    #[cfg(unix)]
    fn new() -> Result<Self, String> {
        use tokio::signal::unix::{SignalKind, signal};
        let listen = |kind| signal(kind).map_err(|e| format!("cannot handle signals: {e}"));
        Ok(Self {
            interrupt: listen(SignalKind::interrupt())?,
            terminate: listen(SignalKind::terminate())?,
        })
    }

    #[cfg(not(unix))]
    fn new() -> Result<Self, String> {
        Ok(Self {})
    }

    #[cfg(unix)]
    async fn recv(&mut self) {
        tokio::select! {
            _ = self.interrupt.recv() => {}
            _ = self.terminate.recv() => {}
        }
    }

    #[cfg(not(unix))]
    async fn recv(&mut self) {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ca_files_may_be_pem_or_der() {
        let tmp = tempfile::tempdir().unwrap();
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["ca.test".to_owned()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        let write = |name: &str, bytes: &[u8]| {
            let path = tmp.path().join(name);
            std::fs::write(&path, bytes).unwrap();
            path
        };
        let pem = write("ca.pem", format!("{0}{0}", cert.pem()).as_bytes());
        assert_eq!(
            read_ca(&pem).unwrap(),
            [cert.der().clone(), cert.der().clone()]
        );
        let der = write("ca.der", cert.der());
        assert_eq!(read_ca(&der).unwrap(), [cert.der().clone()]);

        let empty = write("empty.pem", b"-----BEGIN PRIVATE KEY-----\n");
        assert!(read_ca(&empty).is_err());
        let huge = write("huge.der", &vec![0; MAX_CA_FILE_BYTES as usize + 1]);
        assert!(read_ca(&huge).unwrap_err().contains("too large"));
        assert!(read_ca(&tmp.path().join("missing")).is_err());
    }

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
