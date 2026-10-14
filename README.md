# Wayfetch Downloader

A planned Rust download manager with a reusable engine, a self-hosted server UI,
and a desktop application sharing the same engine and UI.

This repository currently contains the design guidance and a bootstrapped Cargo
workspace with minimal `wayfetch-domain`, `wayfetch-protocol`,
`wayfetch-engine`, and `wayfetch-net` library crates. The domain crate defines typed IDs, job states,
source identity, and failure scopes; the protocol crate defines commands, queries,
errors, and events; the engine owns a locked state directory and a SQLite store
that commits commands durably and recovers jobs after a restart. The engine can
download a direct link into private staging, resume it after a restart, publish
it into a configured root, and stream bounded events, with every queue, buffer,
and deadline set in `EngineConfig`. It reaches the network only through a
`Transport` the host supplies; `wayfetch-net` provides one over HTTP(S), with
TLS verification, an egress policy checked on every redirect, no
decompression, and a broker that never repeats a request its transfer lease
does not allow. The `wayfetch` command-line host (`crates/cli`) runs the engine
in a foreground session; no server, desktop host, or frontend package exists
yet.

## Repository structure

```text
wayfetch-downloader/
├── Cargo.toml / Cargo.lock  # Workspace manifest and locked dependencies
├── rust-toolchain.toml      # Pinned Rust toolchain
├── .github/workflows/       # Hygiene, Rust, and dependency-boundary CI
├── scripts/                 # Maintained check scripts used by CI
├── apps/
│   ├── wayfetchd/           # Headless server and bundled UI host
│   ├── wayfetch-cli/        # CLI client
│   ├── wayfetch-desktop/    # Desktop host embedding the engine
│   ├── adapter-worker/     # Isolated provider adapter worker
│   └── extraction-worker/  # Archive extraction worker
├── crates/
│   ├── domain/             # IDs, states, validated domain types
│   ├── protocol/           # Shared commands, results, errors, events
│   ├── engine/             # Reusable engine and lifecycle
│   ├── store/              # Persistence and migrations
│   ├── net/                # HTTP, sessions, streaming transfers
│   ├── cli/                # Foreground `wayfetch` host
│   ├── fs/                 # Safe paths, checkpoints, publication
│   ├── adapters/           # Adapter SDK and provider integrations
│   ├── extraction/         # Archive workflow and worker supervision
│   └── api/                # HTTP authentication and REST/SSE adapter
├── ui/src/
│   ├── components/         # Shared workflow views
│   ├── client/             # Client contract and DTO bindings
│   ├── transports/         # HTTP/SSE and desktop IPC adapters
│   └── shell/              # Optional native integration
├── tests/
│   ├── fixtures/
│   ├── integration/
│   ├── embedding/
│   ├── recovery/
│   ├── security/
│   ├── performance/
│   └── packaging/
├── deploy/                 # Future deployment configuration
├── docs/architecture.md    # Canonical design and decision register
├── AGENTS.md               # Agent workflow and repository policy
├── LICENSE-APACHE          # Apache License 2.0
├── LICENSE-MIT             # MIT License
└── CONTRIBUTING.md         # Contribution guidance
```

The empty implementation directories are retained with `.gitkeep` files for this
requested structure-only bootstrap. Remove each marker when real files are added.
The architecture's decisions remain in the single architecture brief; additional
documentation directories can be introduced when maintained content needs them.

The product name is **Wayfetch Downloader** and the repository slug is
`wayfetch-downloader`. Proposed component identifiers such as `wayfetch-engine`,
`wayfetch-protocol`, `wayfetchd`, and the `wayfetch` CLI remain as specified in the
architecture.

## Using the `wayfetch` CLI

`wayfetch` runs the engine in the foreground and takes commands on standard
input. Build it with `cargo build --locked -p wayfetch-cli`
(`target/debug/wayfetch`).

```sh
# Once per root: mark an existing directory as the download root. A session
# never does this itself, so a missing mount is never mistaken for an empty
# directory.
wayfetch init-root --state-dir /srv/wayfetch/state /srv/downloads

# A session. Paste or pipe URLs, one or more per line.
wayfetch run --state-dir /srv/wayfetch/state --download-root /srv/downloads
```

In a session, a line holding URLs downloads them, and `list`, `inspect JOB`,
`pause JOB`, `resume JOB`, `cancel JOB`, `help`, and `quit` do what they say
(`JOB` is `job-1` or `1`). `quit`, end of input, Ctrl+C, or `SIGTERM` shut the
engine down gracefully: nothing is cancelled, and unfinished jobs resume from
their durable bytes on the next start. Pause and cancel keep partial data. One
process owns a state directory at a time; a second `run` or `init-root` on it
is refused, and controlling a running session from another process is future
daemon work. `--json` prints one JSON object per line for scripts. Output
never holds up the session or its shutdown: if the reader stops reading, lines
beyond a small queue are dropped and the count is reported once there is room.

URLs are read only from standard input, so they never appear in process
listings, and output never repeats a submitted URL or rejected input. Lines
over 16 KiB are discarded. Only public addresses are reachable and TLS is
always verified against the platform's roots; `--allow-network CIDR` and
`--ca-cert FILE` (PEM or DER) add explicit exceptions, for example a local
server with its own CA. Live transfer rates are not reported yet; `inspect`
and `list` show durable progress, which advances at each 8 MiB checkpoint.

## Working on the project

Read [CONTRIBUTING.md](CONTRIBUTING.md), [AGENTS.md](AGENTS.md), and the relevant
sections of [the architecture brief](docs/architecture.md) before implementation.
The brief describes intended behavior and acceptance gates, not existing features.

### Developer commands

You need `rustup`, Python 3, and a C compiler (the engine builds its bundled
SQLite). The Rust toolchain is pinned in
`rust-toolchain.toml`; `rustup toolchain install` (or the first `cargo` command)
installs it with rustfmt and Clippy. No JavaScript or desktop tooling is needed
for the engine. These are the checks CI runs on every push and pull request:

```sh
cargo fmt --all -- --check                                   # formatting
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features
cargo build --locked -p wayfetch-engine                      # engine alone
python3 scripts/check_dependency_boundaries.py               # crate boundaries
python3 -m unittest discover -s scripts                       # tests for the check
```

`scripts/check_dependency_boundaries.py` fails if the domain, protocol, engine,
net, or CLI crates gain a server, desktop, or UI-toolkit dependency (for example Axum,
Tauri, or GTK), if the domain and protocol crates depend on the engine, or if the
engine depends on the net crate. It checks
every target with all features enabled, so optional dependencies count too. CI
also runs a repository hygiene check that rejects tracked agent-output
directories.

The HTTP tests start their own HTTP and HTTPS servers on `127.0.0.1`, with a
certificate generated per test; they need no network access. The crash tests
(`crates/net/tests/process_recovery.rs`, Unix only) run the engine in a child
process, a rerun of their own test binary, and kill it with `SIGKILL` at named
boundaries from the engine's test-only `failpoints` feature. The CLI tests
(`crates/cli/tests/session.rs`) drive the built `wayfetch` executable against
the same fixture servers.

Use `cargo fmt --all` to apply formatting.

## License

Wayfetch Downloader is dual-licensed under [MIT](LICENSE-MIT) or
[Apache-2.0](LICENSE-APACHE), at your option.
