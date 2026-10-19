//! What every Wayfetch Downloader application host composes the same way
//! (architecture brief section 4.3, "application host"): the operator's
//! configuration file, state-directory, root, network, and credential-key
//! options, explicit root initialization, engine startup with the HTTP
//! transport and account integrations, signals, and non-blocking output.
//!
//! Hosts differ only in how they talk to people: `wayfetch` reads standard
//! input, `wayfetchd` serves HTTP, and `wayfetch-desktop` shows the shared
//! UI in a window over in-process IPC. Neither initializes a root implicitly,
//! and both leave persistence, scheduling, and recovery to the engine.

pub mod config;
mod signals;
mod terminal;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::Args;
use serde::de::{DeserializeOwned, IgnoredAny};
use wayfetch_adapters::ffmpeg::{self, Ffmpeg};
use wayfetch_adapters::plugin::{Describe, find_in, find_on_path};
use wayfetch_adapters::plugins::{Plugins, Spec};
use wayfetch_adapters::release;
use wayfetch_adapters::youtube::YouTube;
use wayfetch_adapters::ytdlp::{self, YtDlp};
use wayfetch_engine::fs::Root;
use wayfetch_engine::state_dir::StateDir;
use wayfetch_engine::{EngineBuilder, EngineConfig, EngineOwner};
use wayfetch_net::ipnet::IpNet;
use wayfetch_net::{CertificateDer, HttpConfig, HttpTransport};

pub use config::{Loaded, NoServer};
pub use signals::Signals;
pub use terminal::{Closed, DRAIN, Message, Printer, Terminal};

/// How long a host waits, after its work is done, for blocking threads it
/// no longer needs before it exits anyway.
pub const RUNTIME_RELEASE: Duration = Duration::from_secs(1);

/// Largest CA certificate file accepted.
const MAX_CA_FILE_BYTES: u64 = 1024 * 1024;

/// The configuration file and state directory options.
#[derive(Debug, Default, Args)]
pub struct StateArgs {
    /// Read settings from this TOML configuration file at startup. Options
    /// given on the command line replace its values; repeatable ones add
    /// to its lists. It must exist.
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,
    /// The engine's state directory. Its parent must exist; one engine owns
    /// it at a time. Replaces `storage.state_dir`.
    #[arg(long, value_name = "DIR")]
    pub state_dir: Option<PathBuf>,
}

impl StateArgs {
    /// Reads the configuration file, if one is named.
    pub fn load<S: DeserializeOwned>(&self) -> Result<Option<Loaded<S>>, String> {
        self.config.as_deref().map(config::load).transpose()
    }

    /// The state directory: the command line's, else the file's.
    fn state_dir<S>(&self, loaded: Option<&Loaded<S>>) -> Result<PathBuf, String> {
        match (
            &self.state_dir,
            loaded.and_then(|l| l.file.storage.state_dir.as_ref()),
        ) {
            (Some(dir), _) => absolute(dir),
            (None, Some(dir)) => Ok(dir.clone()),
            (None, None) => {
                Err("no state directory: pass --state-dir or set storage.state_dir".to_owned())
            }
        }
    }
}

/// Arguments of `init-root`.
#[derive(Debug, Args)]
pub struct InitRootArgs {
    /// The state directory.
    #[command(flatten)]
    pub state: StateArgs,
    /// The existing directory to mark as a destination: the download root,
    /// or another folder given with `--root`.
    pub root: PathBuf,
}

/// Options for running an engine.
#[derive(Debug, Default, Args)]
pub struct EngineArgs {
    /// The state directory.
    #[command(flatten)]
    pub state: StateArgs,
    /// The download root, initialized with `init-root`: where downloads are
    /// saved until the owner chooses another default. Replaces
    /// `storage.download_root`.
    #[arg(long, value_name = "DIR")]
    pub download_root: Option<PathBuf>,
    /// Another folder the owner may choose as the default destination,
    /// initialized with `init-root`. Repeatable; adds to `storage.roots`.
    #[arg(long = "root", value_name = "DIR")]
    pub roots: Vec<PathBuf>,
    /// Also allow downloads from this non-public network, for example
    /// `192.168.1.0/24` for a local server. Only public addresses are
    /// reachable by default. Repeatable; adds to
    /// `network.allow_networks`.
    #[arg(long, value_name = "CIDR")]
    pub allow_network: Vec<IpNet>,
    /// Also trust this certificate authority (PEM or DER) for HTTPS, in
    /// addition to the platform's roots. Certificate verification cannot be
    /// turned off. Repeatable; adds to `network.ca_certs`.
    #[arg(long, value_name = "FILE")]
    pub ca_cert: Vec<PathBuf>,
    /// The key file that encrypts provider account credentials: 64 hex
    /// digits, readable by its owner only, outside the state directory.
    /// Created when the first account is added if it does not exist then,
    /// and never replaced afterwards. `wayfetchd` defaults to
    /// `credential-key` beside its token file, the desktop app to a file
    /// beside the state directory; without one, accounts cannot be added.
    /// Downloads work either way.
    #[arg(long, value_name = "FILE")]
    pub credential_key: Option<PathBuf>,
    /// Another address serving a provider's API, for testing against a
    /// local stand-in: HTTPS, or HTTP on a loopback address. Repeatable.
    #[arg(
        long = "provider-endpoint",
        value_name = "SERVICE=URL",
        value_parser = service_and_url,
        hide = true
    )]
    pub provider_endpoints: Vec<(String, String)>,
    /// A folder of plugin programs shipped with the application, used
    /// where the configuration names none and before `PATH`: `yt-dlp`,
    /// `ffmpeg`, and `deno`, which yt-dlp is then told to run. Set by an
    /// installed host, never on the command line.
    #[arg(skip)]
    pub bundled_plugins: Option<PathBuf>,
}

fn service_and_url(value: &str) -> Result<(String, String), String> {
    let (service, url) = value
        .split_once('=')
        .ok_or("expected SERVICE=URL, such as real-debrid=http://127.0.0.1:9000/")?;
    Ok((service.to_owned(), url.to_owned()))
}

impl EngineArgs {
    /// Uses a key file beside the state directory, named after it, unless
    /// one was given: for a desktop, where one person owns both.
    pub fn default_credential_key(&mut self) -> Result<(), String> {
        if self.credential_key.is_none() {
            let loaded = self.state.load::<IgnoredAny>()?;
            let state = self.state.state_dir(loaded.as_ref())?;
            let name = state
                .file_name()
                .ok_or_else(|| format!("{}: not a directory name", state.display()))?;
            let mut key = name.to_owned();
            key.push(".credential-key");
            self.credential_key = Some(state.with_file_name(key));
        }
        Ok(())
    }
}

/// What a host runs its engine with: the command line applied over the
/// configuration file, checked.
#[derive(Debug)]
pub struct EngineSettings {
    config: EngineConfig,
    http: HttpConfig,
    root: PathBuf,
    provider_endpoints: Vec<(String, String)>,
    plugins: PluginPaths,
}

/// Where the helper programs are, as configured or found on `PATH`.
#[derive(Debug, Clone, Default)]
struct PluginPaths {
    yt_dlp: Option<PathBuf>,
    yt_dlp_js_runtimes: Option<String>,
    ffmpeg: Option<PathBuf>,
    updates: bool,
}

impl EngineSettings {
    /// The engine's state directory.
    pub fn state_dir(&self) -> &Path {
        &self.config.state_dir
    }
}

impl EngineArgs {
    /// Reads the configuration file, if one is named, and applies the
    /// command line over it. `S` is the host's own `[server]` section;
    /// the file is returned for it.
    pub fn load<S: DeserializeOwned>(&self) -> Result<(EngineSettings, Option<Loaded<S>>), String> {
        let loaded = self.state.load::<S>()?;
        let settings = self.settings(loaded.as_ref())?;
        Ok((settings, loaded))
    }

    fn settings<S>(&self, loaded: Option<&Loaded<S>>) -> Result<EngineSettings, String> {
        let file = loaded.map(|l| &l.file);
        let storage = file.map(|f| &f.storage);
        let root = match (
            &self.download_root,
            storage.and_then(|s| s.download_root.as_ref()),
        ) {
            (Some(root), _) => absolute(root)?,
            (None, Some(root)) => root.clone(),
            (None, None) => {
                return Err(
                    "no download root: pass --download-root or set storage.download_root"
                        .to_owned(),
                );
            }
        };
        let mut config = EngineConfig::new(self.state.state_dir(loaded)?);
        config.roots.push(root.clone());
        let file_roots = storage.map(|s| s.roots.as_slice()).unwrap_or_default();
        let cli_roots = self.roots.iter().map(|r| absolute(r));
        for extra in file_roots.iter().cloned().map(Ok).chain(cli_roots) {
            let extra = extra?;
            if !config.roots.contains(&extra) {
                config.roots.push(extra);
            }
        }
        config.download_root = Some(root.clone());
        config.credential_key = self.credential_key.as_deref().map(absolute).transpose()?;
        if let Some(file) = file {
            config::apply(file, &mut config)?;
        }
        let mut http = HttpConfig::default();
        let network = file.map(|f| &f.network);
        http.egress.allowed_networks.extend(
            network
                .iter()
                .flat_map(|n| &n.allow_networks)
                .chain(&self.allow_network)
                .copied(),
        );
        let file_cas = network.iter().flat_map(|n| n.ca_certs.iter().cloned());
        for path in file_cas.chain(self.ca_cert.iter().cloned()) {
            http.extra_roots.extend(read_ca(&path)?);
        }
        let plugins = file.map(|f| &f.plugins);
        let bundled = |name| {
            self.bundled_plugins
                .as_deref()
                .and_then(|dir| find_in(dir, name))
        };
        let find = |name| bundled(name).or_else(|| find_on_path(name));
        let plugins = PluginPaths {
            yt_dlp: plugins
                .and_then(|p| p.yt_dlp.clone())
                .or_else(|| find("yt-dlp")),
            yt_dlp_js_runtimes: plugins
                .and_then(|p| p.yt_dlp_js_runtimes.clone())
                .or_else(|| bundled("deno").map(|deno| format!("deno:{}", deno.display()))),
            ffmpeg: plugins
                .and_then(|p| p.ffmpeg.clone())
                .or_else(|| find("ffmpeg")),
            updates: plugins.and_then(|p| p.updates).unwrap_or(true),
        };
        Ok(EngineSettings {
            config,
            http,
            root,
            provider_endpoints: self.provider_endpoints.clone(),
            plugins,
        })
    }
}

/// Runs `future` to completion on a new [`runtime`], then [`release`]s it.
pub fn block_on<F: Future>(future: F) -> Result<F::Output, String> {
    let runtime = runtime()?;
    let output = runtime.block_on(future);
    release(runtime);
    Ok(output)
}

/// A new multi-threaded runtime for a host's engine and adapters.
pub fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start the runtime: {e}"))
}

/// Shuts `runtime` down without waiting more than [`RUNTIME_RELEASE`] for
/// blocking threads. Dropping a runtime waits for every blocking thread, so
/// one stuck in a filesystem call, such as a read on a hung mount, would
/// otherwise keep the process alive after a graceful shutdown finished.
/// Whatever such a thread was doing is already reported unfinished by the
/// engine's shutdown, and exiting ends it.
pub fn release(runtime: tokio::runtime::Runtime) {
    runtime.shutdown_timeout(RUNTIME_RELEASE);
}

/// Initializes a destination root for the instance in the state directory,
/// creating the instance if the state directory is new. Takes the state
/// directory's lock, so it is refused while an engine owns it. Returns the
/// root's absolute path.
pub fn init_root(args: &InitRootArgs) -> Result<PathBuf, String> {
    // Only the state directory matters here, whichever host the file is for.
    let loaded = args.state.load::<IgnoredAny>()?;
    let state_dir = args.state.state_dir(loaded.as_ref())?;
    let state = StateDir::open(&state_dir).map_err(|e| e.to_string())?;
    let root = absolute(&args.root)?;
    wayfetch_engine::initialize_root(&state, &root)
        .map_err(|e| format!("cannot initialize {}: {e}", root.display()))?;
    Ok(root)
}

/// A started engine and the absolute path of its download root.
#[derive(Debug)]
pub struct Started {
    /// The engine.
    pub owner: EngineOwner,
    /// The download root.
    pub root: PathBuf,
}

impl Started {
    /// Why the download root cannot be used yet, if it cannot. The engine
    /// blocks jobs on an unusable root by itself; this only tells the
    /// operator early, and never initializes anything.
    pub fn root_problem(&self) -> Option<String> {
        self.root_check()().err()
    }

    /// A reusable check of the download root: `Err` with the reason while
    /// it cannot be used, for example after its mount disappeared.
    /// Blocking; it touches the filesystem.
    pub fn root_check(&self) -> impl Fn() -> Result<(), String> + Send + Sync + 'static {
        let (root, instance) = (self.root.clone(), self.owner.identity().instance);
        move || {
            let id = Root::identify(&root, instance)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "it is not initialized, or its drive is not mounted".to_owned())?;
            Root::open(&root, id, instance)
                .map(drop)
                .map_err(|e| e.to_string())
        }
    }
}

/// Starts an engine on the current runtime with the HTTP transport, the
/// production network defaults, and only the exceptions the operator named.
pub async fn start_engine(settings: EngineSettings) -> Result<Started, String> {
    let EngineSettings {
        config,
        http,
        root,
        provider_endpoints,
        plugins,
    } = settings;
    let transport =
        Arc::new(HttpTransport::new(http).map_err(|e| format!("cannot configure HTTP: {e}"))?);
    let providers = wayfetch_adapters::providers(transport.clone(), &provider_endpoints)?;
    // yt-dlp keeps the player code it read in the state directory, so
    // checks after the first are faster.
    let cache = config.state_dir.join("cache").join("yt-dlp");
    let specs = vec![
        Spec {
            describe: ytdlp::PLUGIN,
            check: YtDlp::check(Some(&cache)),
            found: plugins.yt_dlp,
            release: plugins.updates.then_some(&release::YT_DLP),
        },
        Spec {
            describe: ffmpeg::PLUGIN,
            check: Ffmpeg::check(),
            found: plugins.ffmpeg,
            release: None,
        },
    ];
    let control = Arc::new(
        Plugins::open(
            config.state_dir.join("plugins"),
            specs,
            Some(transport.clone()),
        )
        .await?,
    );
    let plugin = |describe: Describe| control.get(describe.id).expect("a plugin named above");
    let ytdlp = YtDlp::new(
        plugin(ytdlp::PLUGIN),
        Some(cache),
        plugins.yt_dlp_js_runtimes,
    );
    let mut builder = EngineBuilder::new(config, tokio::runtime::Handle::current())
        .transport(transport.clone())
        .inspector(transport)
        .media_service(Arc::new(YouTube::new(Arc::new(ytdlp))))
        .converter(Arc::new(Ffmpeg::new(plugin(ffmpeg::PLUGIN))))
        .plugin_control(control.clone());
    for provider in providers {
        builder = builder.account_provider(provider);
    }
    let owner = builder.start().await.map_err(|e| e.to_string())?;
    // The engine holds the state directory now, until the sweep's file
    // work ends.
    control.sweep(&owner.plugin_work()).await?;
    Ok(Started { owner, root })
}

/// The certificates in a PEM file, or the single certificate in a DER file.
fn read_ca(path: &Path) -> Result<Vec<CertificateDer<'static>>, String> {
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

/// `path` made absolute against the working directory, without resolving
/// symbolic links.
pub fn absolute(path: &Path) -> Result<PathBuf, String> {
    std::path::absolute(path).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stuck_blocking_thread_does_not_hold_up_exit() {
        let (_release, stuck) = std::sync::mpsc::channel::<()>();
        let started = std::time::Instant::now();
        let output = block_on(async move {
            tokio::task::spawn_blocking(move || stuck.recv());
            7
        })
        .unwrap();
        assert_eq!(output, 7);
        assert!(
            started.elapsed() < RUNTIME_RELEASE * 3,
            "{:?}",
            started.elapsed()
        );
    }

    #[derive(clap::Parser)]
    struct Run {
        #[command(flatten)]
        engine: EngineArgs,
    }

    fn engine_args(args: &[&str]) -> EngineArgs {
        use clap::Parser;
        Run::try_parse_from(["run"].iter().chain(args))
            .unwrap()
            .engine
    }

    #[test]
    fn the_command_line_replaces_file_values_and_adds_to_its_lists() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("wayfetch.toml");
        std::fs::write(
            &config,
            r#"
schema_version = 1
[storage]
state_dir = "state"
download_root = "downloads"
roots = ["media"]
[network]
allow_networks = ["10.0.0.0/8"]
[downloads.max_active]
max = 8
"#,
        )
        .unwrap();
        let config = config.to_str().unwrap();
        let dir = absolute(tmp.path()).unwrap();

        let (settings, _) = engine_args(&["--config", config])
            .load::<NoServer>()
            .unwrap();
        assert_eq!(settings.state_dir(), dir.join("state"));
        assert_eq!(settings.root, dir.join("downloads"));
        assert_eq!(
            settings.config.roots,
            [dir.join("downloads"), dir.join("media")]
        );
        assert_eq!(settings.config.policy.max_active_downloads.max, 8);
        assert_eq!(settings.http.egress.allowed_networks.len(), 1);

        let elsewhere = dir.join("elsewhere");
        let (settings, _) = engine_args(&[
            "--config",
            config,
            "--state-dir",
            elsewhere.to_str().unwrap(),
            "--root",
            "/srv/more",
            "--allow-network",
            "192.168.0.0/16",
        ])
        .load::<NoServer>()
        .unwrap();
        assert_eq!(settings.state_dir(), elsewhere);
        assert_eq!(
            settings.config.roots,
            [dir.join("downloads"), dir.join("media"), "/srv/more".into()]
        );
        assert_eq!(settings.http.egress.allowed_networks.len(), 2);
    }

    #[test]
    fn without_a_file_the_command_line_must_name_the_directories() {
        let refused = engine_args(&["--download-root", "d"])
            .load::<NoServer>()
            .unwrap_err();
        assert!(refused.contains("--state-dir"), "{refused}");
        let refused = engine_args(&["--state-dir", "s"])
            .load::<NoServer>()
            .unwrap_err();
        assert!(refused.contains("--download-root"), "{refused}");
        let (settings, loaded) = engine_args(&["--state-dir", "s", "--download-root", "d"])
            .load::<NoServer>()
            .unwrap();
        assert!(loaded.is_none());
        assert_eq!(settings.config.policy, wayfetch_engine::Policy::DEFAULT);
    }

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
    fn bundled_plugins_come_after_the_file_and_tell_yt_dlp_about_deno() {
        let dir = tempfile::tempdir().unwrap();
        let bundled = dir.path().join("plugins");
        std::fs::create_dir(&bundled).unwrap();
        for name in ["yt-dlp", "deno"] {
            std::fs::write(bundled.join(name), "").unwrap();
        }
        let file = dir.path().join("wayfetch.toml");
        std::fs::write(
            &file,
            "schema_version = 1\n[storage]\nstate_dir = 's'\ndownload_root = 'd'\n",
        )
        .unwrap();
        let mut args = engine_args(&["--config", file.to_str().unwrap()]);
        args.bundled_plugins = Some(bundled.clone());
        let (settings, _) = args.load::<NoServer>().unwrap();
        assert_eq!(settings.plugins.yt_dlp, Some(bundled.join("yt-dlp")));
        assert_eq!(
            settings.plugins.yt_dlp_js_runtimes,
            Some(format!("deno:{}", bundled.join("deno").display()))
        );
        // Not bundled: found on PATH or not at all, as without a folder.
        assert_eq!(settings.plugins.ffmpeg, find_on_path("ffmpeg"));

        std::fs::write(
            &file,
            "schema_version = 1\n[storage]\nstate_dir = 's'\ndownload_root = 'd'\n\
             [plugins]\nyt_dlp = '/opt/yt-dlp'\nyt_dlp_js_runtimes = 'node'\n",
        )
        .unwrap();
        let (settings, _) = args.load::<NoServer>().unwrap();
        assert_eq!(settings.plugins.yt_dlp, Some("/opt/yt-dlp".into()));
        assert_eq!(settings.plugins.yt_dlp_js_runtimes.as_deref(), Some("node"));
    }

    #[test]
    fn desktop_key_and_providers_survive_loading_toml_settings() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("wayfetch.toml");
        std::fs::write(
            &file,
            "schema_version = 1\n[storage]\nstate_dir = 'state'\ndownload_root = 'downloads'\n",
        )
        .unwrap();
        let mut args = engine_args(&[
            "--config",
            file.to_str().unwrap(),
            "--provider-endpoint",
            "real-debrid=http://127.0.0.1:9000/",
        ]);
        args.default_credential_key().unwrap();
        let (settings, _) = args.load::<NoServer>().unwrap();
        assert_eq!(
            settings.config.credential_key,
            Some(dir.path().join("state.credential-key"))
        );
        assert_eq!(
            settings.provider_endpoints,
            vec![("real-debrid".into(), "http://127.0.0.1:9000/".into())]
        );
    }
}
