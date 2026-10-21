//! `wayfetch`: a foreground command-line host. It owns the runtime,
//! configuration, signals, and terminal, and turns typed lines into engine
//! commands; everything else stays in the engine.

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
    /// Mark an existing directory as a destination of this state directory:
    /// the download root, or a folder given with `--root`.
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
    wayfetch_host::launch_confined_if_asked();
    let cli = Cli::parse();
    let terminal = Terminal::start();
    let result = match cli.command {
        Command::InitRoot(args) => wayfetch_host::init_root(&args).map(|root| {
            terminal.line(format!("initialized download root {}", root.display()));
            ExitCode::SUCCESS
        }),
        Command::Run(args) => {
            wayfetch_host::block_on(session::run(args, &terminal)).and_then(|result| result)
        }
    };
    let code = result.unwrap_or_else(|message| {
        terminal.fatal(&message);
        ExitCode::FAILURE
    });
    // A reader that stopped reading cannot hold up exit.
    terminal.finish(wayfetch_host::DRAIN);
    code
}
