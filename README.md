# Wayfetch Downloader

A planned Rust download manager with a reusable engine, a self-hosted server UI,
and a desktop application sharing the same engine and UI.

This repository currently contains the design guidance and a bootstrapped Cargo
workspace with minimal `wayfetch-domain`, `wayfetch-protocol`, and
`wayfetch-engine` library crates. No download behavior, application hosts,
frontend packages, or runnable product builds exist yet.

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

## Working on the project

Read [CONTRIBUTING.md](CONTRIBUTING.md), [AGENTS.md](AGENTS.md), and the relevant
sections of [the architecture brief](docs/architecture.md) before implementation.
The brief describes intended behavior and acceptance gates, not existing features.

### Developer commands

You need `rustup` and Python 3. The Rust toolchain is pinned in
`rust-toolchain.toml`; `rustup toolchain install` (or the first `cargo` command)
installs it with rustfmt and Clippy. No JavaScript or desktop tooling is needed
for the engine. These are the checks CI runs on every push and pull request:

```sh
cargo fmt --all -- --check                                   # formatting
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo build --locked -p wayfetch-engine                      # engine alone
python3 scripts/check_dependency_boundaries.py               # crate boundaries
```

`scripts/check_dependency_boundaries.py` fails if the domain, protocol, or engine
crates gain a server, desktop, or UI-toolkit dependency (for example Axum, Tauri,
or GTK), or if the domain and protocol crates depend on the engine. CI also runs
a repository hygiene check that rejects tracked agent-output directories.

Use `cargo fmt --all` to apply formatting. The crates have no behavior yet, so
`cargo test` currently runs zero tests.

## License

Wayfetch Downloader is dual-licensed under [MIT](LICENSE-MIT) or
[Apache-2.0](LICENSE-APACHE), at your option.
