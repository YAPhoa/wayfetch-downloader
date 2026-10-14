//! `wayfetch`: a foreground command-line host for the Wayfetch Downloader
//! engine (architecture brief sections 1.1 and 4.3, "application host").
//!
//! The host owns the Tokio runtime, the engine's lifecycle, its
//! configuration, signals, and the terminal. Everything about persistence,
//! scheduling, transfers, recovery, and cleanup stays in the engine; the
//! session only turns typed lines into engine commands and queries and
//! prints what comes back. Controls exist only inside the running session:
//! a second invocation on the same state directory is refused, and talking
//! to another process is later daemon work.

mod input;
mod output;
mod session;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use wayfetch_engine::domain::id::RootId;
use wayfetch_engine::fs::Root;
use wayfetch_engine::state_dir::StateDir;
use wayfetch_net::ipnet::IpNet;

/// The one destination root this host configures.
const DOWNLOAD_ROOT: RootId = RootId::from_u64(1).unwrap();

/// Direct downloads with the Wayfetch Downloader engine.
#[derive(Debug, Parser)]
#[command(name = "wayfetch", version)]
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
    InitRoot {
        #[command(flatten)]
        state: StateArgs,
        /// The existing directory downloads are published in.
        root: PathBuf,
    },
    /// Run a foreground session that reads commands from standard input.
    ///
    /// Type a URL (or `add URL...`) to download it; `list`, `inspect JOB`,
    /// `pause JOB`, `resume JOB`, and `cancel JOB` control jobs; `quit`,
    /// end of input, or Ctrl+C shut down gracefully, keeping unfinished
    /// jobs for the next start. URLs are read from standard input only, so
    /// they never appear in process listings.
    Run(RunArgs),
}

#[derive(Debug, Args)]
struct StateArgs {
    /// The engine's state directory. Its parent must exist; one engine owns
    /// it at a time.
    #[arg(long, value_name = "DIR")]
    state_dir: PathBuf,
}

#[derive(Debug, Args)]
struct RunArgs {
    #[command(flatten)]
    state: StateArgs,
    /// The download root, initialized with `init-root`.
    #[arg(long, value_name = "DIR")]
    download_root: PathBuf,
    /// Also allow downloads from this non-public network, for example
    /// `192.168.1.0/24` for a local server. Only public addresses are
    /// reachable by default. Repeatable.
    #[arg(long, value_name = "CIDR")]
    allow_network: Vec<IpNet>,
    /// Also trust this certificate authority (PEM or DER) for HTTPS, in
    /// addition to the platform's roots. Certificate verification cannot be
    /// turned off. Repeatable.
    #[arg(long, value_name = "FILE")]
    ca_cert: Vec<PathBuf>,
    /// Print one JSON object per line instead of text.
    #[arg(long)]
    json: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::InitRoot { state, root } => init_root(&state.state_dir, &root),
        Command::Run(args) => tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("cannot start the runtime: {e}"))
            .and_then(|runtime| runtime.block_on(session::run(args))),
    };
    match result {
        Ok(code) => code,
        Err(message) => {
            output::report_fatal(&message);
            ExitCode::FAILURE
        }
    }
}

/// Initializes `root` for the instance in `state_dir`, creating the
/// instance if the state directory is new. Takes the state directory's
/// lock, so it is refused while an engine owns it.
fn init_root(state_dir: &Path, root: &Path) -> Result<ExitCode, String> {
    let state = StateDir::open(&absolute(state_dir)?).map_err(|e| e.to_string())?;
    let root = absolute(root)?;
    Root::initialize(&root, DOWNLOAD_ROOT, state.identity().instance)
        .map_err(|e| format!("cannot initialize {}: {e}", root.display()))?;
    println!("initialized download root {}", root.display());
    Ok(ExitCode::SUCCESS)
}

/// `path` made absolute against the working directory, without resolving
/// symbolic links.
fn absolute(path: &Path) -> Result<PathBuf, String> {
    std::path::absolute(path).map_err(|e| format!("{}: {e}", path.display()))
}
