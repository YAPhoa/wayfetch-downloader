//! Installing a newer release the way a browser does: the engine's update
//! check finds it, this downloads and verifies its package in the
//! background through Tauri's updater, and the window offers to restart
//! into it. Installing a .deb or .rpm needs the administrator, so the
//! system asks for the password then.
//!
//! The updater runs from here only; the window gets none of its commands.
//! It reads `latest.json` from the newest published release and checks
//! the package's signature against the public key in `tauri.conf.json`. A
//! build without that key, or one not installed from a package, only
//! links to the release.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use tauri::{AppHandle, Runtime};
use tauri_plugin_updater::{Error, Update, UpdaterExt};
use wayfetch_engine::domain::name::DiagnosticText;
use wayfetch_engine::protocol::update::UpdateInstall;
use wayfetch_engine::protocol::{ErrorCode, ProtocolError};
use wayfetch_host::Installer;

/// Gets the newer release ready, then installs it on request. Clones
/// share it.
#[derive(Debug, Clone, Default)]
pub struct Updates(Arc<Inner>);

#[derive(Debug, Default)]
struct Inner {
    /// Set once the window's app exists.
    app: OnceLock<AppHandle>,
    state: Mutex<State>,
    /// Whether the app starts again once it has stopped.
    relaunch: AtomicBool,
}

#[derive(Default)]
enum State {
    #[default]
    Idle,
    Downloading {
        version: String,
        received: u64,
        total: Option<u64>,
    },
    Ready {
        version: String,
        update: Box<Update>,
        package: Arc<Vec<u8>>,
    },
    Installing {
        version: String,
    },
    Failed {
        version: String,
        problem: String,
    },
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Idle => "Idle",
            Self::Downloading { .. } => "Downloading",
            Self::Ready { .. } => "Ready",
            Self::Installing { .. } => "Installing",
            Self::Failed { .. } => "Failed",
        })
    }
}

impl State {
    fn version(&self) -> Option<&str> {
        match self {
            Self::Idle => None,
            Self::Downloading { version, .. }
            | Self::Ready { version, .. }
            | Self::Installing { version }
            | Self::Failed { version, .. } => Some(version),
        }
    }
}

impl Updates {
    /// Whether this build can install updates: one installed from a
    /// package, with the release signing key built in.
    pub fn supported(config: &tauri::Config) -> bool {
        let signed = config
            .plugins
            .0
            .get("updater")
            .and_then(|updater| updater.get("pubkey"))
            .and_then(|key| key.as_str())
            .is_some_and(|key| !key.trim().is_empty());
        let packaged = matches!(
            tauri::utils::platform::bundle_type(),
            Some(tauri::utils::config::BundleType::Deb | tauri::utils::config::BundleType::Rpm)
        );
        signed && packaged
    }

    /// Starts getting releases ready, through `app`.
    pub fn attach(&self, app: AppHandle) {
        let _ = self.0.app.set(app);
    }

    /// Whether the app starts again once it has stopped.
    pub fn relaunching(&self) -> bool {
        self.0.relaunch.load(Ordering::SeqCst)
    }

    /// Installs the release that is ready, which asks for the
    /// administrator's password, then stops the app to start it again.
    pub async fn restart_to_update(&self) -> Result<(), ProtocolError> {
        let (version, update, package) = {
            let mut state = self.0.state.lock().unwrap();
            let State::Ready { version, .. } = &*state else {
                return Err(ProtocolError::new(
                    ErrorCode::NotApplicable,
                    "no update is ready to install",
                ));
            };
            let version = version.clone();
            let State::Ready {
                version,
                update,
                package,
            } = std::mem::replace(&mut *state, State::Installing { version })
            else {
                unreachable!("checked above");
            };
            (version, update, package)
        };
        let installing = {
            let (update, package) = (update.clone(), package.clone());
            tokio::task::spawn_blocking(move || update.install(&*package)).await
        };
        let problem = match installing {
            Ok(Ok(())) => {
                self.0.relaunch.store(true, Ordering::SeqCst);
                if let Some(app) = self.0.app.get() {
                    app.exit(0);
                }
                return Ok(());
            }
            Ok(Err(Error::AuthenticationFailed | Error::PackageInstallFailed)) => {
                "the update was not installed: the system did not allow it".to_owned()
            }
            Ok(Err(e)) => format!("the update was not installed: {e}"),
            Err(_) => "the update was not installed".to_owned(),
        };
        // Ready again, so the person can try once more.
        *self.0.state.lock().unwrap() = State::Ready {
            version,
            update,
            package,
        };
        Err(ProtocolError::new(ErrorCode::Internal, &problem))
    }

    async fn download<R: Runtime>(self, app: AppHandle<R>, version: String) {
        let problem = match self.fetch(&app, &version).await {
            Ok(()) => return,
            Err(problem) => problem,
        };
        let mut state = self.0.state.lock().unwrap();
        if state.version() == Some(&version) {
            *state = State::Failed { version, problem };
        }
    }

    async fn fetch<R: Runtime>(&self, app: &AppHandle<R>, version: &str) -> Result<(), String> {
        let update = app
            .updater()
            .map_err(|e| e.to_string())?
            .check()
            .await
            .map_err(|e| explain(&e))?
            .ok_or("no package newer than this app was published")?;
        let progress = self.0.clone();
        let package = update
            .download(
                move |chunk, total| {
                    if let State::Downloading {
                        received, total: t, ..
                    } = &mut *progress.state.lock().unwrap()
                    {
                        *received += chunk as u64;
                        *t = total;
                    }
                },
                || {},
            )
            .await
            .map_err(|e| explain(&e))?;
        let mut state = self.0.state.lock().unwrap();
        if state.version() == Some(version) {
            *state = State::Ready {
                version: version.to_owned(),
                update: Box::new(update),
                package: Arc::new(package),
            };
        }
        Ok(())
    }
}

/// Why a package could not be got ready, in plain words.
fn explain(error: &Error) -> String {
    match error {
        Error::ReleaseNotFound | Error::TargetNotFound(_) | Error::TargetsNotFound(_) => {
            "this release has no package for this app".to_owned()
        }
        Error::Minisign(_) | Error::SignatureUtf8(_) | Error::Base64(_) => {
            "the downloaded package is not signed by Wayfetch's release key".to_owned()
        }
        Error::Reqwest(_) | Error::Network(_) => {
            "the package could not be downloaded; check again later".to_owned()
        }
        other => other.to_string(),
    }
}

impl Installer for Updates {
    fn prepare(&self, version: &str) {
        let Some(app) = self.0.app.get().cloned() else {
            return;
        };
        {
            let mut state = self.0.state.lock().unwrap();
            let underway =
                state.version() == Some(version) && !matches!(*state, State::Failed { .. });
            if underway || matches!(*state, State::Installing { .. }) {
                return;
            }
            *state = State::Downloading {
                version: version.to_owned(),
                received: 0,
                total: None,
            };
        }
        tauri::async_runtime::spawn(self.clone().download(app, version.to_owned()));
    }

    fn state(&self) -> Option<UpdateInstall> {
        Some(match &*self.0.state.lock().unwrap() {
            State::Idle => return None,
            State::Downloading {
                received, total, ..
            } => UpdateInstall::Downloading {
                received: *received,
                total: *total,
            },
            State::Ready { .. } => UpdateInstall::Ready,
            State::Installing { .. } => UpdateInstall::Installing,
            State::Failed { problem, .. } => UpdateInstall::Failed {
                problem: DiagnosticText::new(problem),
            },
        })
    }
}

/// Starts this app again, with the arguments it was started with: after an
/// update, the newly installed one.
pub fn relaunch(env: &tauri::Env) {
    let Ok(binary) = tauri::process::current_binary(env) else {
        return;
    };
    let _ = std::process::Command::new(binary)
        .args(env.args_os.iter().skip(1))
        .spawn();
}

/// Lets the window's app reach the updater.
pub fn plugin<R: Runtime>() -> tauri::plugin::TauriPlugin<R, tauri_plugin_updater::Config> {
    tauri_plugin_updater::Builder::new().build()
}
