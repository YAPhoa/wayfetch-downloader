//! `wayfetchd`: the headless Wayfetch Downloader daemon (architecture
//! brief sections 4.1, 4.3 and 8.5).
//!
//! A thin host: it owns the Tokio runtime, the engine's lifecycle, the
//! listener, and process signals, and serves the engine through the
//! authenticated HTTP/SSE adapter in `wayfetch-api`. Jobs belong to the
//! engine, not to connections: a client that disconnects, crashes, or
//! never reads its event stream changes nothing about downloads. `SIGINT`
//! or `SIGTERM` drains the API and shuts the engine down gracefully,
//! keeping unfinished jobs for the next start.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use wayfetch_api::{Api, ApiLimits, Tokens};
use wayfetch_engine::protocol::InstanceId;
use wayfetch_engine::{ShutdownReport, ShutdownRequest};
use wayfetch_host::{EngineArgs, InitRootArgs, Message, Printer, Signals, Terminal};

/// Loopback only, so nothing beyond this machine reaches the API unless
/// the operator says so.
const DEFAULT_LISTEN: &str = "127.0.0.1:7878";

/// The Wayfetch Downloader daemon.
#[derive(Debug, Parser)]
#[command(name = "wayfetchd", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Mark an existing directory as this state directory's download root.
    ///
    /// An explicit operator action, done once per root: `run` never
    /// initializes a root, so a missing mount is never mistaken for an
    /// empty directory.
    InitRoot(InitRootArgs),
    /// Run the engine and serve its HTTP API until SIGINT or SIGTERM.
    Run(RunArgs),
}

#[derive(Debug, Args)]
struct RunArgs {
    #[command(flatten)]
    engine: EngineArgs,
    /// Address to serve the API on. Loopback by default; anything else
    /// should sit behind an authenticated TLS reverse proxy.
    #[arg(long, value_name = "ADDR", default_value = DEFAULT_LISTEN)]
    listen: SocketAddr,
    /// File of API bearer tokens, one `<token> <permission>[,...]` per
    /// line, readable only by its owner. Created with one token holding
    /// every permission if missing.
    #[arg(long, value_name = "FILE")]
    token_file: PathBuf,
    /// Print one JSON object per line instead of text.
    #[arg(long)]
    json: bool,
}

/// What the daemon prints.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Notice {
    /// A token file was created; the token is in it, never in output.
    TokenFileCreated { path: PathBuf },
    /// The API is being served.
    Listening {
        address: SocketAddr,
        instance: InstanceId,
    },
    /// The API is reachable beyond this machine.
    NotLoopback { address: SocketAddr },
    /// The download root cannot be used yet.
    RootUnavailable { reason: String },
    /// Shutdown started.
    ShuttingDown { reason: &'static str },
    /// Output was dropped because nothing read it fast enough.
    Dropped { count: u64 },
    /// The daemon stopped.
    Stopped {
        /// Whether every connection closed and every engine task finished
        /// in time.
        clean: bool,
        /// Whether every API connection closed in time.
        connections_closed: bool,
        /// Engine tasks still running after the deadline.
        unfinished_tasks: usize,
        /// Whether the state directory was released.
        state_dir_released: bool,
    },
}

impl Message for Notice {
    fn text(&self) -> Option<String> {
        Some(match self {
            Self::TokenFileCreated { path } => format!(
                "created API token file {} with one token holding every permission",
                path.display()
            ),
            Self::Listening { address, instance } => {
                format!("wayfetchd serving http://{address}/v1 (instance {instance})")
            }
            Self::NotLoopback { address } => format!(
                "warning: {address} is reachable beyond this machine without TLS; put an \
                 authenticated TLS reverse proxy in front of it"
            ),
            Self::RootUnavailable { reason } => format!(
                "warning: the download root cannot be used ({reason}); downloads wait until it \
                 can. Initialize a new root with `wayfetchd init-root`."
            ),
            Self::ShuttingDown { reason } => format!("shutting down ({reason})"),
            Self::Dropped { count } => {
                format!("{count} line(s) of output dropped: the reader fell behind")
            }
            Self::Stopped { clean: true, .. } => {
                "stopped; unfinished jobs continue on the next start".to_owned()
            }
            Self::Stopped {
                connections_closed,
                unfinished_tasks,
                ..
            } => format!(
                "stopped uncleanly: connections closed: {connections_closed}, \
                 {unfinished_tasks} engine task(s) still running"
            ),
        })
    }

    fn is_problem(&self) -> bool {
        matches!(
            self,
            Self::NotLoopback { .. } | Self::RootUnavailable { .. } | Self::Dropped { .. }
        )
    }

    fn dropped(count: u64) -> Self {
        Self::Dropped { count }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let terminal = Terminal::start();
    let result = match cli.command {
        Command::InitRoot(args) => wayfetch_host::init_root(&args).map(|root| {
            terminal.line(format!("initialized download root {}", root.display()));
            ExitCode::SUCCESS
        }),
        Command::Run(args) => tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("cannot start the runtime: {e}"))
            .and_then(|runtime| runtime.block_on(run(args, &terminal))),
    };
    let code = result.unwrap_or_else(|message| {
        terminal.fatal(&message);
        ExitCode::FAILURE
    });
    terminal.finish(wayfetch_host::DRAIN);
    code
}

/// Serves until a signal, then drains the API and stops the engine.
async fn run(args: RunArgs, terminal: &Terminal) -> Result<ExitCode, String> {
    let printer = Printer::new(args.json, terminal);
    // A closed output never stops the daemon; it only loses its log.
    let print = |notice: &Notice| {
        let _ = printer.print(notice);
    };
    let mut signals = Signals::new()?;
    let (tokens, created) = Tokens::load_or_create(&args.token_file).map_err(|e| e.to_string())?;
    if created {
        print(&Notice::TokenFileCreated {
            path: args.token_file.clone(),
        });
    }
    // Bound before the engine starts, so a taken address fails fast and
    // leaves the state directory untouched.
    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .map_err(|e| format!("cannot listen on {}: {e}", args.listen))?;
    let address = listener
        .local_addr()
        .map_err(|e| format!("cannot listen on {}: {e}", args.listen))?;
    let started = wayfetch_host::start_engine(&args.engine).await?;
    let instance = started.owner.identity().instance;
    if let Some(reason) = started.root_problem() {
        print(&Notice::RootUnavailable { reason });
    }
    if !address.ip().is_loopback() {
        print(&Notice::NotLoopback { address });
    }
    print(&Notice::Listening { address, instance });

    let owner = started.owner;
    let api = Api::new(owner.handle(), tokens, ApiLimits::DEFAULT);
    let connections_closed = api
        .serve(listener, async {
            signals.recv().await;
            print(&Notice::ShuttingDown { reason: "signal" });
        })
        .await;
    let report = owner.shutdown(ShutdownRequest::default()).await;
    let stopped = stopped(connections_closed, &report);
    print(&stopped);
    Ok(match stopped {
        Notice::Stopped { clean: true, .. } => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    })
}

fn stopped(connections_closed: bool, report: &ShutdownReport) -> Notice {
    Notice::Stopped {
        clean: connections_closed && report.clean,
        connections_closed,
        unfinished_tasks: report.unfinished_tasks,
        state_dir_released: report.state_dir_released,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_api_listens_on_loopback_by_default() {
        let cli = Cli::try_parse_from([
            "wayfetchd",
            "run",
            "--state-dir",
            "s",
            "--download-root",
            "d",
            "--token-file",
            "t",
        ])
        .unwrap();
        let Command::Run(args) = cli.command else {
            panic!("not run");
        };
        assert!(args.listen.ip().is_loopback());
    }

    #[test]
    fn a_token_file_is_required() {
        let missing = Cli::try_parse_from([
            "wayfetchd",
            "run",
            "--state-dir",
            "s",
            "--download-root",
            "d",
        ]);
        assert!(missing.is_err());
    }
}
