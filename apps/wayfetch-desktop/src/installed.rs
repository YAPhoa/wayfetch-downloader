//! Starting the installed app from the desktop's app menu, with no
//! arguments.
//!
//! It uses the platform's data and download folders. The download folder is
//! initialized only on the first start, so a later missing drive is reported
//! rather than replaced. Start failures are also shown in a message box.

use std::path::PathBuf;
use std::process::ExitCode;

use wayfetch_host::{EngineArgs, InitRootArgs, StateArgs};

use crate::RunArgs;

/// The application's folder name in the platform's data folder.
const APP_DIR: &str = "wayfetch-downloader";

/// Where an installed package keeps its plugin programs, relative to the
/// folder of the executable: `/usr/bin` and `/usr/lib/wayfetch-downloader/
/// plugins` in the Linux packages (see `tauri.conf.json`).
const PLUGINS_DIR: &str = "../lib/wayfetch-downloader/plugins";

/// Opens the window on the platform's folders, setting them up on a
/// first start.
pub fn launch() -> ExitCode {
    match defaults().and_then(|mut args| crate::run(&mut args)) {
        Ok(code) => code,
        Err(message) => {
            show_failure(&message);
            ExitCode::FAILURE
        }
    }
}

/// The plugin folder shipped beside this executable, if there is one.
pub fn plugins_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    // Shown in Settings > Plugins, so without the `..`.
    let dir = exe.parent()?.join(PLUGINS_DIR).canonicalize().ok()?;
    dir.is_dir().then_some(dir)
}

fn defaults() -> Result<RunArgs, String> {
    let no_home = || "cannot find the home folder: HOME is not set".to_owned();
    let data = dirs::data_dir().ok_or_else(no_home)?;
    let downloads = dirs::download_dir()
        .or_else(|| dirs::home_dir().map(|home| home.join("Downloads")))
        .ok_or_else(no_home)?;
    let state_dir = data.join(APP_DIR);
    let state = || StateArgs {
        state_dir: Some(state_dir.clone()),
        ..StateArgs::default()
    };
    let first_start = !state_dir
        .try_exists()
        .map_err(|e| format!("{}: {e}", state_dir.display()))?;
    if first_start {
        for dir in [&data, &downloads] {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        wayfetch_host::init_root(&InitRootArgs {
            state: state(),
            root: downloads.clone(),
        })?;
    }
    Ok(RunArgs {
        engine: EngineArgs {
            state: state(),
            download_root: Some(downloads),
            ..EngineArgs::default()
        },
        json: false,
    })
}

fn show_failure(message: &str) {
    rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Error)
        .set_title("Wayfetch Downloader")
        .set_description(format!("Wayfetch Downloader could not start.\n\n{message}"))
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
}
