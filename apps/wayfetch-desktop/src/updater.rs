//! Installing a newer release when the person asks: the engine's update
//! check finds it, Download update has this download and verify its package
//! in the background through Tauri's updater, and the window then offers to
//! restart into it. Installing a .deb or .rpm needs the administrator, so
//! the system asks for the password then.
//!
//! The updater runs from here only; the window gets none of its commands.
//! It reads `latest.json` from the release the check found, pre-releases
//! included, refuses one for any other version, and checks the package's
//! signature against the public key in `tauri.conf.json`. A
//! build without that key, or one not installed from a package, only
//! links to the release.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tauri::{AppHandle, Runtime, Url};
use tauri_plugin_updater::{Error, Update, UpdaterExt};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use wayfetch_engine::domain::name::DiagnosticText;
use wayfetch_engine::protocol::update::UpdateInstall;
use wayfetch_engine::protocol::{ErrorCode, ProtocolError};
use wayfetch_host::Installer;

/// Downloads the newer release, then installs it, each on request. Clones
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
    /// Cancelled as the app stops, ending a download underway.
    stop: CancellationToken,
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

    async fn download<R: Runtime>(self, app: AppHandle<R>, version: String, page: String) {
        let problem = match self.fetch(&app, &version, &page).await {
            Ok(()) => return,
            Err(problem) => problem,
        };
        let mut state = self.0.state.lock().unwrap();
        if state.version() == Some(&version) {
            *state = State::Failed { version, problem };
        }
    }

    /// Reads the manifest of the release at `page`, which must be for
    /// `version`, then downloads and verifies its package within [`LIMITS`].
    async fn fetch<R: Runtime>(
        &self,
        app: &AppHandle<R>,
        version: &str,
        page: &str,
    ) -> Result<(), String> {
        let manifest = manifest_url(page).ok_or("the release's page is not a GitHub release")?;
        let mut update = app
            .updater_builder()
            .endpoints(vec![manifest])
            .map_err(|e| e.to_string())?
            .timeout(MANIFEST_TIMEOUT)
            // Whether the release is the one shown is checked below,
            // with a clearer problem than "no update".
            .version_comparator(|_, _| true)
            .build()
            .map_err(|e| e.to_string())?
            .check()
            .await
            .map_err(|e| explain(&e))?
            .ok_or("this release has no package for this app")?;
        if !same_version(&update.version, version) {
            return Err(format!(
                "the release's package is version {}, not {version}",
                update.version
            ));
        }
        // A backstop for the whole download; `bounded` says why first.
        update.timeout = Some(LIMITS.deadline + LIMITS.stalled);
        let watch = Arc::new(Watch::new(LIMITS.size));
        let (progress, chunks) = (self.0.clone(), watch.clone());
        let downloading = update.download(
            move |chunk, total| {
                if let State::Downloading {
                    received, total: t, ..
                } = &mut *progress.state.lock().unwrap()
                {
                    *received += chunk as u64;
                    *t = total;
                    chunks.chunk(*received, total);
                }
            },
            || {},
        );
        let downloading = async { downloading.await.map_err(|e| explain(&e)) };
        let package = bounded(downloading, &watch, LIMITS, &self.0.stop).await?;
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

    /// Ends a download underway, as the app stops.
    pub fn stop(&self) {
        self.0.stop.cancel();
    }
}

/// How long reading a release's manifest may take.
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Bounds on downloading a package, which the updater holds in memory
/// until its signature is checked.
#[derive(Debug, Clone, Copy)]
struct Limits {
    /// The largest package taken; Wayfetch's are tens of megabytes.
    size: u64,
    /// How long a download may go without receiving anything.
    stalled: Duration,
    /// How long a whole download may take.
    deadline: Duration,
}

const LIMITS: Limits = Limits {
    size: 256 * 1024 * 1024,
    stalled: Duration::from_secs(60),
    deadline: Duration::from_secs(30 * 60),
};

/// Where the release whose page is `page` keeps its manifest:
/// `…/releases/tag/v0.2.0` keeps it at `…/releases/download/v0.2.0/latest.json`.
fn manifest_url(page: &str) -> Option<Url> {
    let url = Url::parse(page).ok()?;
    let (repository, tag) = url.path().rsplit_once("/releases/tag/")?;
    if tag.is_empty() || tag.contains('/') {
        return None;
    }
    let mut manifest = url.clone();
    manifest.set_path(&format!("{repository}/releases/download/{tag}/latest.json"));
    manifest.set_query(None);
    manifest.set_fragment(None);
    Some(manifest)
}

/// Whether two versions are the same, either written with a leading `v`.
fn same_version(a: &str, b: &str) -> bool {
    let parse = |text: &str| semver::Version::parse(text.strip_prefix('v').unwrap_or(text)).ok();
    parse(a).is_some_and(|a| Some(a) == parse(b))
}

/// What a download has received, as its chunks arrive.
#[derive(Debug)]
struct Watch {
    size: u64,
    last: Mutex<Instant>,
    oversized: AtomicBool,
    wake: Notify,
}

impl Watch {
    fn new(size: u64) -> Self {
        Self {
            size,
            last: Mutex::new(Instant::now()),
            oversized: AtomicBool::new(false),
            wake: Notify::new(),
        }
    }

    /// `received` bytes have arrived so far, of `total` if it was said.
    fn chunk(&self, received: u64, total: Option<u64>) {
        *self.last.lock().unwrap() = Instant::now();
        if received > self.size || total.is_some_and(|total| total > self.size) {
            self.oversized.store(true, Ordering::SeqCst);
            self.wake.notify_one();
        }
    }
}

fn oversized(limits: Limits) -> String {
    format!(
        "the package is larger than {} MB, more than any Wayfetch release",
        limits.size / 1024 / 1024
    )
}

/// `work`, ended early when the package is too large, stops arriving, takes
/// too long, or the app stops.
async fn bounded<T>(
    work: impl Future<Output = Result<T, String>>,
    watch: &Watch,
    limits: Limits,
    stop: &CancellationToken,
) -> Result<T, String> {
    let deadline = Instant::now() + limits.deadline;
    let watchdog = async {
        loop {
            let stalled = *watch.last.lock().unwrap() + limits.stalled;
            tokio::select! {
                () = watch.wake.notified() => {}
                () = tokio::time::sleep_until(stalled.min(deadline).into()) => {}
            }
            if watch.oversized.load(Ordering::SeqCst) {
                return oversized(limits);
            }
            let now = Instant::now();
            if now >= deadline {
                return "the download took too long; try again later".to_owned();
            }
            if now >= *watch.last.lock().unwrap() + limits.stalled {
                return "the download stopped receiving data; try again later".to_owned();
            }
        }
    };
    tokio::select! {
        // The last chunk can cross the limit as the download completes.
        result = work => {
            if watch.oversized.load(Ordering::SeqCst) {
                Err(oversized(limits))
            } else {
                result
            }
        }
        problem = watchdog => Err(problem),
        () = stop.cancelled() => Err("the app stopped before the download finished".to_owned()),
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

impl Updates {
    /// Starts downloading `version`, the release at `page`, in the
    /// background, unless it is already on its way or ready. One that
    /// failed starts again.
    pub fn download_update(&self, version: &str, page: &str) {
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
        tauri::async_runtime::spawn(self.clone().download(
            app,
            version.to_owned(),
            page.to_owned(),
        ));
    }
}

impl Installer for Updates {
    fn state(&self) -> UpdateInstall {
        match &*self.0.state.lock().unwrap() {
            State::Idle => UpdateInstall::Idle,
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
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    const SHORT: Limits = Limits {
        size: 100,
        stalled: Duration::from_millis(200),
        deadline: Duration::from_secs(5),
    };

    #[test]
    fn the_manifest_is_read_from_the_release_found() {
        let page = "https://github.com/YAPhoa/wayfetch-downloader/releases/tag/v0.2.0-beta.1";
        assert_eq!(
            manifest_url(page).unwrap().as_str(),
            "https://github.com/YAPhoa/wayfetch-downloader/releases/download/v0.2.0-beta.1/latest.json"
        );
        assert_eq!(
            manifest_url("https://github.com/YAPhoa/wayfetch-downloader/releases"),
            None
        );
        assert_eq!(manifest_url("https://github.com/o/r/releases/tag/"), None);
        assert_eq!(manifest_url("not a page"), None);
    }

    #[test]
    fn only_the_version_found_is_installed() {
        assert!(same_version("0.2.0", "0.2.0"));
        assert!(same_version("v0.2.0-beta.1", "0.2.0-beta.1"));
        assert!(!same_version("0.1.1", "0.2.0"));
        assert!(!same_version("0.2.0-beta.1", "0.2.0"));
        assert!(!same_version("latest", "latest"));
    }

    #[tokio::test]
    async fn a_download_that_keeps_arriving_finishes() {
        let watch = Watch::new(SHORT.size);
        let work = async {
            for received in [40, 80] {
                tokio::time::sleep(Duration::from_millis(100)).await;
                watch.chunk(received, Some(80));
            }
            Ok("package")
        };
        let finished = bounded(work, &watch, SHORT, &CancellationToken::new()).await;
        assert_eq!(finished, Ok("package"));
    }

    #[tokio::test]
    async fn a_package_larger_than_any_release_is_refused() {
        // Said up front, before anything is held.
        let watch = Watch::new(SHORT.size);
        let work = async {
            watch.chunk(1, Some(1_000));
            std::future::pending::<Result<(), String>>().await
        };
        let refused = bounded(work, &watch, SHORT, &CancellationToken::new()).await;
        assert!(refused.unwrap_err().contains("larger than"));
        // Or found out as it arrives.
        let watch = Watch::new(SHORT.size);
        let work = async {
            watch.chunk(101, None);
            std::future::pending::<Result<(), String>>().await
        };
        let refused = bounded(work, &watch, SHORT, &CancellationToken::new()).await;
        assert!(refused.unwrap_err().contains("larger than"));
        // Or by the last chunk, as the download completes.
        let watch = Watch::new(SHORT.size);
        let work = async {
            watch.chunk(101, None);
            Ok("package")
        };
        let refused = bounded(work, &watch, SHORT, &CancellationToken::new()).await;
        assert!(refused.unwrap_err().contains("larger than"));
    }

    #[tokio::test]
    async fn a_stalled_or_slow_download_ends() {
        let watch = Watch::new(SHORT.size);
        let stalled = bounded(
            std::future::pending::<Result<(), String>>(),
            &watch,
            SHORT,
            &CancellationToken::new(),
        )
        .await;
        assert!(stalled.unwrap_err().contains("stopped receiving"));

        let slow = Limits {
            deadline: Duration::from_millis(300),
            ..SHORT
        };
        let watch = Watch::new(slow.size);
        let work = async {
            // Never stalls, never finishes.
            loop {
                tokio::time::sleep(Duration::from_millis(50)).await;
                watch.chunk(1, None);
            }
        };
        let ended = bounded(work, &watch, slow, &CancellationToken::new()).await;
        assert_eq!(
            ended,
            Err::<(), _>("the download took too long; try again later".to_owned())
        );
    }

    #[tokio::test]
    async fn stopping_the_app_ends_a_download() {
        let watch = Watch::new(SHORT.size);
        let stop = CancellationToken::new();
        stop.cancel();
        let ended = bounded(
            std::future::pending::<Result<(), String>>(),
            &watch,
            SHORT,
            &stop,
        );
        assert!(ended.await.unwrap_err().contains("app stopped"));
    }
}
