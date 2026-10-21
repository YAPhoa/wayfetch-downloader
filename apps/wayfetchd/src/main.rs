//! `wayfetchd`: the headless daemon. It owns the runtime, the engine, the
//! listener, and signals, and serves the engine and the bundled web UI
//! through `wayfetch-api`.
//!
//! Jobs belong to the engine, not to connections, and `SIGINT` or `SIGTERM`
//! keeps unfinished jobs for the next start. Subcommands cover `run`,
//! `init-root`, `ready` (a health check), and `reset-owner`; `--config`
//! reads a TOML file (see `wayfetch_host::config`).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use rust_embed::Embed;
use serde::{Deserialize, Serialize};
use url::Url;
use wayfetch_api::{
    Api, ApiLimits, BrowserAuth, BrowserConfig, SetupIssued, Tokens, UiAssets, UiAssetsError,
};
use wayfetch_engine::protocol::InstanceId;
use wayfetch_engine::{ShutdownReport, ShutdownRequest};
use wayfetch_host::{EngineArgs, InitRootArgs, Loaded, Message, Printer, Signals, Terminal};

/// Loopback only, so nothing beyond this machine reaches the API unless
/// the operator says so.
const DEFAULT_LISTEN: &str = "127.0.0.1:7878";

/// How long `ready` waits to connect, and then for each read or write.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
/// Largest readiness response `ready` reads.
const MAX_PROBE_BYTES: u64 = 16 * 1024;

/// The web UI's source files, served as they are: the UI has no build
/// step, so building the daemon needs no JavaScript tooling. TypeScript
/// declarations are only for type checking.
#[derive(Embed)]
#[folder = "../../ui/src"]
#[exclude = "**/.gitkeep"]
#[exclude = "**/*.ts"]
struct UiFiles;

/// The embedded web UI.
fn ui_assets() -> Result<UiAssets, UiAssetsError> {
    UiAssets::new(UiFiles::iter().filter_map(|path| {
        let file = UiFiles::get(&path)?;
        Some((path.into_owned(), file.data))
    }))
}

/// The Wayfetch Downloader daemon.
#[derive(Debug, Parser)]
#[command(name = "wayfetchd", version)]
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
    /// Run the engine and serve its HTTP API until SIGINT or SIGTERM.
    Run(Box<RunArgs>),
    /// Ask a running daemon whether it is ready; exit 0 only if it is.
    ///
    /// For health checks: it starts no engine, takes no lock, and needs no
    /// token.
    Ready(ReadyArgs),
    /// Remove the browser owner account, for example after a forgotten
    /// password, so the next `run` writes a new setup secret.
    ///
    /// Refused while a daemon uses the auth directory. Downloads, jobs,
    /// and API tokens are untouched.
    ResetOwner(ResetOwnerArgs),
}

#[derive(Debug, Args)]
struct ResetOwnerArgs {
    /// The auth directory the daemon runs with.
    #[arg(long, value_name = "DIR")]
    auth_dir: PathBuf,
}

#[derive(Debug, Args)]
struct BrowserArgs {
    /// Owner-only directory for the browser owner account and its setup
    /// secret; created if missing. Browser sign-in needs it and a browser
    /// origin. Replaces `server.auth_dir`.
    #[arg(long, value_name = "DIR")]
    auth_dir: Option<PathBuf>,
    /// An origin browsers use to reach the daemon, such as
    /// `https://wayfetch.example.lan`: the HTTPS reverse proxy's address,
    /// which must pass the `Host` header through. Needs an auth directory.
    /// Repeatable; adds to `server.browser_origins`.
    #[arg(long = "browser-origin", value_name = "URL")]
    origins: Vec<Url>,
    /// Development only: browser sign-in over plain HTTP on loopback, with
    /// cookies that lack `Secure`. Needs a loopback `--listen` and an auth
    /// directory; the origins default to its address and `localhost`.
    #[arg(long)]
    dev_insecure_loopback: bool,
}

/// `[server]` in the configuration file: the listener and browser access,
/// which only this host has. Paths resolve against the file's directory.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerFile {
    /// As `--listen`.
    listen: Option<SocketAddr>,
    /// As `--token-file`.
    token_file: Option<PathBuf>,
    /// As `--auth-dir`.
    auth_dir: Option<PathBuf>,
    /// As `--browser-origin`.
    #[serde(default)]
    browser_origins: Vec<Url>,
}

/// The server settings: the command line applied over `[server]`.
#[derive(Debug)]
struct Server {
    listen: SocketAddr,
    token_file: PathBuf,
    browser: Option<BrowserConfig>,
}

impl RunArgs {
    fn server(&self, loaded: Option<&Loaded<ServerFile>>) -> Result<Server, String> {
        let empty = ServerFile::default();
        let file = loaded
            .and_then(|l| l.file.server.as_ref())
            .unwrap_or(&empty);
        let from_file = |path: &Option<PathBuf>| {
            path.as_deref()
                .zip(loaded)
                .map(|(path, loaded)| loaded.path(path))
        };
        let token_file = match (&self.token_file, from_file(&file.token_file)) {
            (Some(path), _) => path.clone(),
            (None, Some(path)) => path,
            (None, None) => {
                return Err("no token file: pass --token-file or set server.token_file".to_owned());
            }
        };
        let auth_dir = self
            .browser
            .auth_dir
            .clone()
            .or_else(|| from_file(&file.auth_dir));
        let origins: Vec<Url> = file
            .browser_origins
            .iter()
            .chain(&self.browser.origins)
            .cloned()
            .collect();
        let browser = match auth_dir {
            Some(auth_dir) => Some(BrowserConfig {
                auth_dir,
                origins,
                insecure_loopback: self.browser.dev_insecure_loopback,
            }),
            None if !origins.is_empty() || self.browser.dev_insecure_loopback => {
                return Err(
                    "browser sign-in needs an auth directory: pass --auth-dir or set \
                     server.auth_dir"
                        .to_owned(),
                );
            }
            None => None,
        };
        Ok(Server {
            listen: self
                .listen
                .or(file.listen)
                .unwrap_or_else(|| DEFAULT_LISTEN.parse().expect("a socket address")),
            token_file,
            browser,
        })
    }
}

#[derive(Debug, Args)]
struct ReadyArgs {
    /// Address the daemon serves on.
    #[arg(long, value_name = "ADDR", default_value = DEFAULT_LISTEN)]
    address: SocketAddr,
}

#[derive(Debug, Args)]
struct RunArgs {
    #[command(flatten)]
    engine: EngineArgs,
    /// Address to serve the API on: 127.0.0.1:7878 unless configured.
    /// Anything but loopback should sit behind an authenticated TLS
    /// reverse proxy. Replaces `server.listen`.
    #[arg(long, value_name = "ADDR")]
    listen: Option<SocketAddr>,
    /// File of API bearer tokens, one `<token> <permission>[,...]` per
    /// line, readable only by its owner. Created with one token holding
    /// every permission if missing. Replaces `server.token_file`.
    #[arg(long, value_name = "FILE")]
    token_file: Option<PathBuf>,
    #[command(flatten)]
    browser: BrowserArgs,
    /// Print one JSON object per line instead of text.
    #[arg(long)]
    json: bool,
}

impl RunArgs {
    /// Keeps the credential key beside the token file unless one was
    /// given: with the server's other secrets, apart from the state
    /// directory, so accounts need no setup.
    fn default_credential_key(&mut self) -> Result<(), String> {
        if self.engine.credential_key.is_none() {
            let loaded = self.engine.state.load::<ServerFile>()?;
            let server = self.server(loaded.as_ref())?;
            self.engine.credential_key = Some(server.token_file.with_file_name("credential-key"));
        }
        Ok(())
    }
}

/// What the daemon prints.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Notice {
    /// A token file was created; the token is in it, never in output.
    TokenFileCreated { path: PathBuf },
    /// A browser setup secret was written; the secret is in the file,
    /// never in output.
    SetupSecretIssued { path: PathBuf, valid_for_secs: u64 },
    /// Browsers may sign in from these origins.
    BrowserAccess { origins: Vec<String> },
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
            Self::SetupSecretIssued {
                path,
                valid_for_secs,
            } => format!(
                "no owner account yet: complete setup (POST /v1/auth/setup) within {} minutes with \
                 the setup secret in {}; restart for a new one",
                valid_for_secs / 60,
                path.display()
            ),
            Self::BrowserAccess { origins } => {
                format!("browser sign-in from {}", origins.join(", "))
            }
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
    wayfetch_host::launch_confined_if_asked();
    let cli = Cli::parse();
    let terminal = Terminal::start();
    let result = match cli.command {
        Command::InitRoot(args) => wayfetch_host::init_root(&args).map(|root| {
            terminal.line(format!("initialized download root {}", root.display()));
            ExitCode::SUCCESS
        }),
        Command::Ready(args) => probe(&args).map(|()| {
            terminal.line("ready".to_owned());
            ExitCode::SUCCESS
        }),
        Command::Run(args) => {
            wayfetch_host::block_on(run(*args, &terminal)).and_then(|result| result)
        }
        Command::ResetOwner(args) => wayfetch_api::reset_owner(&args.auth_dir)
            .map(|existed| {
                terminal.line(if existed {
                    "owner account removed; the next start writes a new setup secret".to_owned()
                } else {
                    "there was no owner account; the next start writes a setup secret".to_owned()
                });
                ExitCode::SUCCESS
            })
            .map_err(|e| e.to_string()),
    };
    let code = result.unwrap_or_else(|message| {
        terminal.fatal(&message);
        ExitCode::FAILURE
    });
    terminal.finish(wayfetch_host::DRAIN);
    code
}

/// Serves until a signal, then drains the API and stops the engine.
async fn run(mut args: RunArgs, terminal: &Terminal) -> Result<ExitCode, String> {
    args.default_credential_key()?;
    let printer = Printer::new(args.json, terminal);
    // A closed output never stops the daemon; it only loses its log.
    let print = |notice: &Notice| {
        let _ = printer.print(notice);
    };
    let mut signals = Signals::new()?;
    let ui = ui_assets().map_err(|e| e.to_string())?;
    let (settings, loaded) = args.engine.load::<ServerFile>()?;
    let server = args.server(loaded.as_ref())?;
    let (tokens, created) =
        Tokens::load_or_create(&server.token_file).map_err(|e| e.to_string())?;
    if created {
        print(&Notice::TokenFileCreated {
            path: server.token_file.clone(),
        });
    }
    // Bound before the engine starts, so a taken address fails fast and
    // leaves the state directory untouched.
    let listener = tokio::net::TcpListener::bind(server.listen)
        .await
        .map_err(|e| format!("cannot listen on {}: {e}", server.listen))?;
    let address = listener
        .local_addr()
        .map_err(|e| format!("cannot listen on {}: {e}", server.listen))?;
    // Also before the engine: a bad origin or a busy auth directory
    // leaves the state directory untouched.
    let browser = server
        .browser
        .as_ref()
        .map(|config| BrowserAuth::open(config, address))
        .transpose()
        .map_err(|e| e.to_string())?;
    let started = wayfetch_host::start_engine(settings).await?;
    let instance = started.owner.identity().instance;
    if let Some(reason) = started.root_problem() {
        print(&Notice::RootUnavailable { reason });
    }
    let root_check = started.root_check();
    if !address.ip().is_loopback() {
        print(&Notice::NotLoopback { address });
    }
    if let Some((browser, issued)) = &browser {
        if let Some(SetupIssued { path, valid_for }) = issued {
            print(&Notice::SetupSecretIssued {
                path: path.clone(),
                valid_for_secs: valid_for.as_secs(),
            });
        }
        print(&Notice::BrowserAccess {
            origins: browser.origins().to_vec(),
        });
    }
    print(&Notice::Listening { address, instance });

    let owner = started.owner;
    let mut api = Api::new(owner.handle(), tokens, ApiLimits::DEFAULT)
        .ui(ui)
        .readiness_check(Arc::new(move || {
            root_check().map_err(|_| "download_root_unavailable")
        }));
    if let Some((browser, _)) = browser {
        api = api.browser_access(browser);
    }
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

/// What `GET /v1/ready` answers.
#[derive(Debug, Deserialize)]
struct ReadyStatus {
    ready: bool,
    reason: Option<String>,
}

/// Asks the daemon at `args.address` whether it is ready. `Err` says why
/// not, or why it could not be asked.
fn probe(args: &ReadyArgs) -> Result<(), String> {
    let fail = |e: &dyn std::fmt::Display| format!("cannot ask {}: {e}", args.address);
    let mut stream =
        TcpStream::connect_timeout(&args.address, PROBE_TIMEOUT).map_err(|e| fail(&e))?;
    stream
        .set_read_timeout(Some(PROBE_TIMEOUT))
        .and_then(|()| stream.set_write_timeout(Some(PROBE_TIMEOUT)))
        .and_then(|()| {
            write!(
                stream,
                "GET /v1/ready HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
                args.address
            )
        })
        .map_err(|e| fail(&e))?;
    let mut response = Vec::new();
    stream
        .take(MAX_PROBE_BYTES)
        .read_to_end(&mut response)
        .map_err(|e| fail(&e))?;
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut head = httparse::Response::new(&mut headers);
    let Ok(httparse::Status::Complete(body)) = head.parse(&response) else {
        return Err(fail(&"malformed response"));
    };
    let status: ReadyStatus =
        serde_json::from_slice(&response[body..]).map_err(|_| fail(&"unexpected response"))?;
    match (head.code, status) {
        (Some(200), ReadyStatus { ready: true, .. }) => Ok(()),
        (_, status) => Err(format!(
            "not ready: {}",
            status.reason.as_deref().unwrap_or("unknown")
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    const RUN: [&str; 6] = [
        "run",
        "--state-dir",
        "s",
        "--download-root",
        "d",
        "--token-file",
    ];

    fn run_args(extra: &[&str]) -> RunArgs {
        let cli = Cli::try_parse_from(["wayfetchd"].iter().chain(extra)).unwrap();
        let Command::Run(args) = cli.command else {
            panic!("not run");
        };
        *args
    }

    fn parse(extra: &[&str]) -> RunArgs {
        run_args(
            &RUN.iter()
                .chain(&["t"])
                .chain(extra)
                .copied()
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn the_api_listens_on_loopback_by_default() {
        assert!(parse(&[]).server(None).unwrap().listen.ip().is_loopback());
    }

    #[test]
    fn browser_options_need_an_auth_directory() {
        for extra in [
            &["--browser-origin", "https://wayfetch.example.lan"][..],
            &["--dev-insecure-loopback"],
        ] {
            let refused = parse(extra).server(None).unwrap_err();
            assert!(refused.contains("auth directory"), "{refused}");
        }
        let server = parse(&["--auth-dir", "a", "--dev-insecure-loopback"])
            .server(None)
            .unwrap();
        assert!(server.browser.unwrap().insecure_loopback);
        assert!(
            parse(&[]).server(None).unwrap().browser.is_none(),
            "browser access is opt-in"
        );
    }

    #[test]
    fn the_web_ui_is_embedded() {
        let ui = ui_assets().expect("ui/src has an index.html");
        assert_eq!(ui.version().len(), 16);
        assert!(UiFiles::get("main.js").is_some());
        assert!(UiFiles::iter().all(|path| !path.ends_with(".gitkeep") && !path.ends_with(".ts")));
    }

    #[test]
    fn the_credential_key_defaults_to_beside_the_token_file() {
        let run = [
            "wayfetchd",
            "run",
            "--state-dir",
            "/srv/state",
            "--download-root",
            "/srv/downloads",
            "--token-file",
            "/srv/secrets/api-tokens",
        ];
        let key = |extra: &[&str]| {
            let Command::Run(mut args) = Cli::try_parse_from(run.iter().chain(extra))
                .unwrap()
                .command
            else {
                panic!("not run");
            };
            args.default_credential_key().unwrap();
            args.engine.credential_key.unwrap()
        };
        assert_eq!(key(&[]), PathBuf::from("/srv/secrets/credential-key"));
        assert_eq!(
            key(&["--credential-key", "/keys/wayfetch"]),
            PathBuf::from("/keys/wayfetch")
        );
    }

    #[test]
    fn a_token_file_is_required() {
        let refused = run_args(&RUN[..5]).server(None).unwrap_err();
        assert!(refused.contains("token file"), "{refused}");
    }

    #[test]
    fn the_command_line_replaces_server_settings_and_adds_origins() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("wayfetch.toml");
        std::fs::write(
            &config,
            r#"
schema_version = 1

[storage]
state_dir = "state"
download_root = "downloads"

[server]
listen = "127.0.0.1:9000"
token_file = "tokens"
auth_dir = "auth"
browser_origins = ["https://wayfetch.example.lan"]
"#,
        )
        .unwrap();
        let config = config.to_str().unwrap();
        let args = run_args(&["run", "--config", config]);
        let (engine, loaded) = args.engine.load::<ServerFile>().unwrap();
        let dir = wayfetch_host::absolute(tmp.path()).unwrap();
        assert_eq!(engine.state_dir(), dir.join("state"));
        let server = args.server(loaded.as_ref()).unwrap();
        assert_eq!(server.listen, "127.0.0.1:9000".parse().unwrap());
        assert_eq!(server.token_file, dir.join("tokens"));
        let browser = server.browser.unwrap();
        assert_eq!(browser.auth_dir, dir.join("auth"));
        assert_eq!(browser.origins.len(), 1);

        let args = run_args(&[
            "run",
            "--config",
            config,
            "--listen",
            "127.0.0.1:9001",
            "--token-file",
            "other",
            "--browser-origin",
            "https://other.example.lan",
        ]);
        let (_, loaded) = args.engine.load::<ServerFile>().unwrap();
        let server = args.server(loaded.as_ref()).unwrap();
        assert_eq!(server.listen, "127.0.0.1:9001".parse().unwrap());
        assert_eq!(server.token_file, PathBuf::from("other"));
        assert_eq!(server.browser.unwrap().origins.len(), 2);
    }

    #[test]
    fn the_example_configuration_is_valid() {
        let example = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../deploy/wayfetch.example.toml"
        );
        let args = run_args(&["run", "--config", example]);
        let (engine, loaded) = args.engine.load::<ServerFile>().unwrap();
        assert_eq!(engine.state_dir(), Path::new("/srv/wayfetch/state"));
        let server = args.server(loaded.as_ref()).unwrap();
        assert!(server.listen.ip().is_loopback());
        assert_eq!(server.browser.unwrap().origins.len(), 1);
    }

    #[test]
    fn unknown_server_settings_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("wayfetch.toml");
        std::fs::write(&config, "schema_version = 1\n[server]\nlisten_on = \"x\"\n").unwrap();
        let args = run_args(&["run", "--config", config.to_str().unwrap()]);
        let refused = args.engine.load::<ServerFile>().unwrap_err();
        assert!(refused.contains("listen_on"), "{refused}");
    }
    #[test]
    fn credential_key_follows_the_configured_token_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("wayfetch.toml");
        std::fs::write(&file, "schema_version = 1\n[storage]\nstate_dir = 'state'\ndownload_root = 'downloads'\n[server]\ntoken_file = 'secrets/tokens'\n").unwrap();
        let mut args = run_args(&["run", "--config", file.to_str().unwrap()]);
        args.default_credential_key().unwrap();
        assert_eq!(
            args.engine.credential_key,
            Some(dir.path().join("secrets/credential-key"))
        );
    }
}
