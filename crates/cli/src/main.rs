//! `wayfetch`: a foreground command-line host for the Wayfetch Downloader
//! engine (architecture brief sections 1.1 and 4.3, "application host").
//!
//! The host owns the Tokio runtime, the engine's lifecycle, its
//! configuration, signals, and the terminal. Everything about persistence,
//! scheduling, transfers, recovery, and cleanup stays in the engine; the
//! session only turns typed lines into engine commands and queries and
//! prints what comes back. Controls exist only inside the running session:
//! a second invocation on the same state directory is refused. `wayfetchd`
//! serves the same engine over HTTP instead.

mod input;
mod output;
mod session;

use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use wayfetch_host::{EngineArgs, InitRootArgs, Terminal};

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
    InitRoot(InitRootArgs),
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
struct RunArgs {
    #[command(flatten)]
    engine: EngineArgs,
    /// Print one JSON object per line instead of text.
    #[arg(long)]
    json: bool,
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
            .and_then(|runtime| runtime.block_on(session::run(args, &terminal))),
    };
    let code = result.unwrap_or_else(|message| {
        terminal.fatal(&message);
        ExitCode::FAILURE
    });
    // A reader that stopped reading cannot hold up exit.
    terminal.finish(wayfetch_host::DRAIN);
    code
}
