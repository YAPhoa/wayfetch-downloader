//! `wayfetch-desktop`: the desktop host. Like `wayfetchd` but with a window
//! instead of a listener (ADR-31 to ADR-34).
//!
//! The engine runs in this process and the shared UI reaches it only through
//! the IPC commands in [`ipc`]; no daemon or listening socket is started.
//! Closing the window or a signal shuts the engine down gracefully, and
//! unfinished jobs continue on the next start.

mod assets;
mod installed;
mod ipc;
mod picker;
mod subscriptions;
mod updater;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::mpsc::{SyncSender, sync_channel};

use clap::{Parser, Subcommand};
use serde::Serialize;
use tauri::webview::PageLoadEvent;
use tauri::{Manager, RunEvent, WindowEvent};
use wayfetch_engine::protocol::InstanceId;
use wayfetch_engine::{ShutdownReport, ShutdownRequest};
use wayfetch_host::{EngineArgs, InitRootArgs, Message, NoServer, Printer, Signals, Terminal};

use crate::ipc::Desktop;
use crate::subscriptions::{Ended, Subscriptions};

/// Notices queued for the terminal from the window's threads.
const NOTICE_QUEUE: usize = 64;

/// The Wayfetch Downloader desktop app.
#[derive(Debug, Parser)]
#[command(name = "wayfetch-desktop", version)]
struct Cli {
    /// Without one, the window opens on the platform's folders, as when
    /// started from the app menu.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Mark an existing directory as a destination of this state directory:
    /// the download root, or a folder given with `--root`.
    ///
    /// An explicit action, done once per root: `run` never initializes a
    /// root, so a missing mount is never mistaken for an empty directory.
    InitRoot(InitRootArgs),
    /// Open the window with an engine on the state directory; closing the
    /// window stops the engine.
    Run(RunArgs),
}

#[derive(Debug, clap::Args)]
struct RunArgs {
    #[command(flatten)]
    engine: EngineArgs,
    /// Print one JSON object per line instead of text.
    #[arg(long)]
    json: bool,
}

/// What the desktop host prints.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Notice {
    /// The engine started and the window is opening.
    Started {
        instance: InstanceId,
        state_dir: PathBuf,
    },
    /// The download root cannot be used yet.
    RootUnavailable { reason: String },
    /// The number of open event subscriptions changed: one opened, or one
    /// ended for `ended`.
    Subscriptions {
        open: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        ended: Option<Ended>,
    },
    /// Shutdown started.
    ShuttingDown { reason: &'static str },
    /// Output was dropped because nothing read it fast enough.
    Dropped { count: u64 },
    /// The host could not run.
    Failed { message: String },
    /// The host stopped.
    Stopped {
        /// Whether every engine task finished in time.
        clean: bool,
        /// Engine tasks still running after the deadline.
        unfinished_tasks: usize,
        /// Whether the state directory was released.
        state_dir_released: bool,
    },
}

impl Message for Notice {
    fn text(&self) -> Option<String> {
        Some(match self {
            Self::Started {
                instance,
                state_dir,
            } => format!(
                "engine started on {} (instance {instance})",
                state_dir.display()
            ),
            Self::RootUnavailable { reason } => format!(
                "warning: the download root cannot be used ({reason}); downloads wait until it \
                 can. Initialize a new root with `wayfetch-desktop init-root`."
            ),
            // Routine; scripts and tests read it with --json.
            Self::Subscriptions { .. } => return None,
            Self::ShuttingDown { reason } => format!("shutting down ({reason})"),
            Self::Dropped { count } => {
                format!("{count} line(s) of output dropped: the reader fell behind")
            }
            Self::Failed { message } => format!("error: {message}"),
            Self::Stopped { clean: true, .. } => {
                "stopped; unfinished jobs continue on the next start".to_owned()
            }
            Self::Stopped {
                unfinished_tasks, ..
            } => format!("stopped uncleanly: {unfinished_tasks} engine task(s) still running"),
        })
    }

    fn is_problem(&self) -> bool {
        matches!(
            self,
            Self::RootUnavailable { .. } | Self::Dropped { .. } | Self::Failed { .. }
        )
    }

    fn dropped(count: u64) -> Self {
        Self::Dropped { count }
    }
}

fn main() -> ExitCode {
    wayfetch_host::launch_confined_if_asked();
    match Cli::parse().command {
        None => installed::launch(),
        Some(Command::InitRoot(args)) => {
            let terminal = Terminal::start();
            let code = match wayfetch_host::init_root(&args) {
                Ok(root) => {
                    terminal.line(format!("initialized download root {}", root.display()));
                    ExitCode::SUCCESS
                }
                Err(message) => {
                    terminal.fatal(&message);
                    ExitCode::FAILURE
                }
            };
            terminal.finish(wayfetch_host::DRAIN);
            code
        }
        Some(Command::Run(mut args)) => run(&mut args).unwrap_or(ExitCode::FAILURE),
    }
}

/// Runs the engine and the window until the window closes or a signal
/// arrives, then stops the engine. A failure to run is printed, and
/// returned for a host that shows it some other way too.
fn run(args: &mut RunArgs) -> Result<ExitCode, String> {
    // The window's threads report through a queue to one thread that owns
    // the terminal; `None` ends it.
    let (notices, queued) = sync_channel::<Option<Notice>>(NOTICE_QUEUE);
    let json = args.json;
    let output = std::thread::spawn(move || {
        let terminal = Terminal::start();
        let printer = Printer::new(json, &terminal);
        // A closed output never stops the host; it only loses its log.
        for notice in queued.iter().map_while(|n| n) {
            let _ = printer.print(&notice);
        }
        terminal.finish(wayfetch_host::DRAIN);
    });
    args.engine.bundled_plugins = installed::plugins_dir();
    let result = args.engine.default_credential_key();
    let result = result
        .and_then(|()| wayfetch_host::runtime())
        .and_then(|runtime| {
            let result = run_on(&runtime, &args.engine, &notices);
            wayfetch_host::release(runtime);
            result
        });
    if let Err(message) = &result {
        notify(
            &notices,
            Notice::Failed {
                message: message.clone(),
            },
        );
    }
    // The printer is draining, so this waits only for room.
    let _ = notices.send(None);
    let _ = output.join();
    result
}

/// Queues a notice; drops it if the printer is behind.
fn notify(notices: &SyncSender<Option<Notice>>, notice: Notice) {
    let _ = notices.try_send(Some(notice));
}

fn run_on(
    runtime: &tokio::runtime::Runtime,
    args: &EngineArgs,
    notices: &SyncSender<Option<Notice>>,
) -> Result<ExitCode, String> {
    // The desktop app has no listener, so a `[server]` section is refused.
    let (settings, _) = args.load::<NoServer>()?;
    let state_dir = settings.state_dir().to_path_buf();
    let mut context = tauri::generate_context!();
    // A build that installs updates gets each newer release ready.
    let updates = updater::Updates::supported(context.config()).then(updater::Updates::default);
    let settings = match &updates {
        Some(updates) => settings.installer(Arc::new(updates.clone())),
        None => settings,
    };
    // Signals first, so one during startup still stops gracefully; the
    // engine before any window, so a refused state directory shows none.
    let (mut signals, started) = runtime.block_on(async {
        let signals = Signals::new()?;
        let started = wayfetch_host::start_engine(settings).await?;
        Ok::<_, String>((signals, started))
    })?;
    let root_problem = started.root_problem();
    let owner = started.owner;
    notify(
        notices,
        Notice::Started {
            instance: owner.identity().instance,
            state_dir,
        },
    );
    if let Some(reason) = root_problem {
        notify(notices, Notice::RootUnavailable { reason });
    }

    // IPC handlers and subscription pumps run on the host's runtime.
    tauri::async_runtime::set(runtime.handle().clone());
    let on_change = {
        let notices = notices.clone();
        Arc::new(move |open, ended| notify(&notices, Notice::Subscriptions { open, ended }))
    };
    let subscriptions = Subscriptions::new(on_change);
    let embedded = context.assets;
    context.assets = Box::new(assets::DesktopAssets(embedded));
    let mut builder = tauri::Builder::default();
    if updates.is_some() {
        builder = builder.plugin(updater::plugin());
    }
    let app = builder
        .manage(Desktop {
            engine: owner.handle(),
            subscriptions: subscriptions.clone(),
            picker: picker::Picker::default(),
            updates: updates.clone(),
        })
        .invoke_handler(tauri::generate_handler![
            ipc::hello,
            ipc::query,
            ipc::submit,
            ipc::job_command,
            ipc::extract,
            ipc::collect_links,
            ipc::links_command,
            ipc::update_settings,
            ipc::remove_root,
            ipc::change_plugin,
            ipc::authorize_account,
            ipc::cancel_authorization,
            ipc::account_command,
            ipc::refresh_account,
            ipc::choose_download_folder,
            ipc::open_file,
            ipc::reveal_file,
            ipc::subscribe,
            ipc::acknowledge,
            ipc::unsubscribe,
            ipc::restart_to_update,
        ])
        .on_page_load(|webview, payload| {
            // The page that held these subscriptions is going away.
            if payload.event() == PageLoadEvent::Started {
                let desktop = webview.state::<Desktop>();
                desktop.subscriptions.close_renderer(webview.label());
            }
        })
        .build(context);
    let app = match app {
        Ok(app) => app,
        Err(e) => {
            let report = runtime.block_on(owner.shutdown(ShutdownRequest::default()));
            notify(notices, stopped(&report));
            return Err(format!("cannot open the window: {e}"));
        }
    };

    let handle = app.handle().clone();
    if let Some(updates) = &updates {
        updates.attach(handle.clone());
    }
    let env = app.env();
    {
        let notices = notices.clone();
        runtime.spawn(async move {
            signals.recv().await;
            notify(&notices, Notice::ShuttingDown { reason: "signal" });
            handle.exit(0);
        });
    }
    let window_notices = notices.clone();
    app.run_return(move |app, event| {
        if let RunEvent::WindowEvent {
            event: WindowEvent::CloseRequested { .. },
            ..
        } = event
        {
            notify(
                &window_notices,
                Notice::ShuttingDown {
                    reason: "window closed",
                },
            );
            // Closing the main window ends the app, even if another window
            // were open.
            app.exit(0);
        }
    });
    // The window is gone, and with it every renderer.
    subscriptions.close_all();
    let report = runtime.block_on(owner.shutdown(ShutdownRequest::default()));
    let stopped = stopped(&report);
    let clean = matches!(stopped, Notice::Stopped { clean: true, .. });
    notify(notices, stopped);
    // An update was installed: start the new version, now that this one
    // has released the state directory.
    if updates.is_some_and(|updates| updates.relaunching()) {
        updater::relaunch(&env);
    }
    Ok(if clean {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn stopped(report: &ShutdownReport) -> Notice {
    Notice::Stopped {
        clean: report.clean,
        unfinished_tasks: report.unfinished_tasks,
        state_dir_released: report.state_dir_released,
    }
}
