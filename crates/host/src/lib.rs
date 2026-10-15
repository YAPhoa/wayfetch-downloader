//! What every Wayfetch Downloader application host composes the same way
//! (architecture brief section 4.3, "application host"): the operator's
//! state-directory, root, and network options, explicit root
//! initialization, engine startup with the HTTP transport, process
//! signals, and non-blocking terminal output.
//!
//! Hosts differ only in how they talk to people: `wayfetch` reads standard
//! input, `wayfetchd` serves HTTP, and `wayfetch-desktop` shows the shared
//! UI in a window over in-process IPC. Neither initializes a root implicitly,
//! and both leave persistence, scheduling, and recovery to the engine.

mod signals;
mod terminal;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::Args;
use wayfetch_engine::fs::Root;
use wayfetch_engine::state_dir::StateDir;
use wayfetch_engine::{EngineBuilder, EngineConfig, EngineOwner};
use wayfetch_net::ipnet::IpNet;
use wayfetch_net::{CertificateDer, HttpConfig, HttpTransport};

pub use signals::Signals;
pub use terminal::{Closed, DRAIN, Message, Printer, Terminal};

/// How long a host waits, after its work is done, for blocking threads it
/// no longer needs before it exits anyway.
pub const RUNTIME_RELEASE: Duration = Duration::from_secs(1);

/// Largest CA certificate file accepted.
const MAX_CA_FILE_BYTES: u64 = 1024 * 1024;

/// The state directory option.
#[derive(Debug, Args)]
pub struct StateArgs {
    /// The engine's state directory. Its parent must exist; one engine owns
    /// it at a time.
    #[arg(long, value_name = "DIR")]
    pub state_dir: PathBuf,
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
#[derive(Debug, Args)]
pub struct EngineArgs {
    /// The state directory.
    #[command(flatten)]
    pub state: StateArgs,
    /// The download root, initialized with `init-root`: where downloads are
    /// saved until the owner chooses another default.
    #[arg(long, value_name = "DIR")]
    pub download_root: PathBuf,
    /// Another folder the owner may choose as the default destination,
    /// initialized with `init-root`. Repeatable.
    #[arg(long = "root", value_name = "DIR")]
    pub roots: Vec<PathBuf>,
    /// Also allow downloads from this non-public network, for example
    /// `192.168.1.0/24` for a local server. Only public addresses are
    /// reachable by default. Repeatable.
    #[arg(long, value_name = "CIDR")]
    pub allow_network: Vec<IpNet>,
    /// Also trust this certificate authority (PEM or DER) for HTTPS, in
    /// addition to the platform's roots. Certificate verification cannot be
    /// turned off. Repeatable.
    #[arg(long, value_name = "FILE")]
    pub ca_cert: Vec<PathBuf>,
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
    let state = StateDir::open(&absolute(&args.state.state_dir)?).map_err(|e| e.to_string())?;
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
pub async fn start_engine(args: &EngineArgs) -> Result<Started, String> {
    let state_dir = absolute(&args.state.state_dir)?;
    let root = absolute(&args.download_root)?;
    let transport = HttpTransport::new(http_config(args)?)
        .map_err(|e| format!("cannot configure HTTP: {e}"))?;
    let mut config = EngineConfig::new(state_dir);
    config.roots.push(root.clone());
    for extra in &args.roots {
        let extra = absolute(extra)?;
        if !config.roots.contains(&extra) {
            config.roots.push(extra);
        }
    }
    config.download_root = Some(root.clone());
    let owner = EngineBuilder::new(config, tokio::runtime::Handle::current())
        .transport(Arc::new(transport))
        .start()
        .await
        .map_err(|e| e.to_string())?;
    Ok(Started { owner, root })
}

/// Transport settings: the production defaults, plus only the exceptions
/// the operator named.
fn http_config(args: &EngineArgs) -> Result<HttpConfig, String> {
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
}
