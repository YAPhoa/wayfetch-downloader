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
in a foreground session, and the `wayfetchd` daemon (`apps/wayfetchd`) serves it
through an authenticated HTTP/SSE API (`crates/api`); both compose the engine
the same way through `crates/host`. The `Dockerfile` and
`deploy/compose.yaml` run the daemon in a least-privilege container. No
desktop host or frontend package exists yet.

## Repository structure

```text
wayfetch-downloader/
├── Cargo.toml / Cargo.lock  # Workspace manifest and locked dependencies
├── rust-toolchain.toml      # Pinned Rust toolchain
├── Dockerfile               # wayfetchd server image
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
│   ├── host/               # What the CLI and daemon hosts share
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
├── deploy/                 # Compose file for the server image
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
is refused; use `wayfetchd` to control downloads from other processes. `--json` prints one JSON object per line for scripts. Output
never holds up the session or its shutdown: if the reader stops reading, lines
beyond a small queue are dropped and the count is reported once there is room.

URLs are read only from standard input, so they never appear in process
listings, and output never repeats a submitted URL or rejected input. Lines
over 16 KiB are discarded. Only public addresses are reachable and TLS is
always verified against the platform's roots; `--allow-network CIDR` and
`--ca-cert FILE` (PEM or DER) add explicit exceptions, for example a local
server with its own CA. Live transfer rates are not reported yet; `inspect`
and `list` show durable progress, which advances at each 8 MiB checkpoint.

## Running the `wayfetchd` daemon

`wayfetchd` runs the engine headless and serves it over HTTP. Build it with
`cargo build --locked -p wayfetchd` (`target/debug/wayfetchd`).

```sh
wayfetchd init-root --state-dir /srv/wayfetch/state /srv/downloads
wayfetchd run --state-dir /srv/wayfetch/state --download-root /srv/downloads \
    --token-file /srv/wayfetch/api-tokens
```

It listens on `127.0.0.1:7878` unless `--listen` says otherwise; anything
beyond loopback has no TLS of its own and belongs behind an authenticated TLS
reverse proxy. Every request needs `Authorization: Bearer TOKEN`. The token file
holds one `TOKEN PERMISSION[,PERMISSION...]` per line, with permissions `read`,
`submit`, and `control`, and must be readable only by its owner. If it does not
exist, `wayfetchd` creates it with one token holding every permission; the token
is never printed.

| Route | Purpose |
|---|---|
| `GET /v1/capabilities` | Instance ID, boot epoch, protocol version, newest event, limits |
| `POST /v1/inputs` | Submit `{"idempotency_key", "target", "items": [URL...]}` |
| `GET /v1/jobs` | A page of jobs: `after`, `page_size`, `stage=a,b`, `package` |
| `GET /v1/jobs/{id}` | One job |
| `POST /v1/jobs/{id}/commands` | `{"idempotency_key", "target", "action": "pause"\|"resume"\|"cancel", "expected_revision"}` |
| `GET /v1/events` | Server-sent events: `hello`, `event`, `progress`, `resync` |
| `GET /v1/ready` | Readiness, without a token: `200 {"ready":true}`, or `503` with a `reason` |

`target` is the instance from `/v1/capabilities` and `idempotency_key` a fresh
UUID per logical command; a retry with the same key returns the first result.
Responses and errors are the engine's protocol types, so `code` values match
every other client. Durable events carry `INSTANCE:POSITION` as their event ID:
reconnecting with `Last-Event-ID` (or `?after=`) replays exactly what was
missed, and a `resync` event ends the stream when that is impossible, after
which the client re-reads `/v1/jobs` and subscribes from its `as_of` cursor.

Request bodies are capped at 1 MiB, connections at 64, and event streams at 16.
A request's headers must arrive within 10 seconds, and its body within 10
seconds after them, so a client that stops sending cannot hold a connection. Closing a connection or stream
never affects a download. `SIGINT` or `SIGTERM` ends event streams, lets
requests in progress finish for up to 5 seconds, and shuts the engine down
gracefully; unfinished jobs continue on the next start.

`GET /v1/ready` is the one route that needs no token, and it reveals only a
fixed reason: `starting`, `shutting_down`, `engine_failed`,
`engine_unavailable`, `download_root_unavailable` (not initialized, or its
mount is gone), or `check_timed_out` (the checks took over 2 seconds, for
example on a hung mount; one check runs at a time, and a stuck one never
holds up shutdown). `wayfetchd ready [--address ADDR]` asks a running daemon and
exits 0 only if it is ready; it starts no engine and reads no token, so it
serves as a health check.

## Running in a container

The image holds only `wayfetchd` on a distroless Debian base with the system
CA bundle: no shell or package manager. It is built with the toolchain in
`rust-toolchain.toml` and `--locked` dependencies. `deploy/compose.yaml` runs
it as a configurable UID/GID with a read-only root filesystem, a 16 MiB
`/tmp`, no capabilities, `no-new-privileges`, a process limit, an init
process, and a 45 second stop grace period. Docker with the Compose plugin
is all the host needs.

```sh
docker build -t wayfetch:local .                 # from the repository root
```

**Directories and permissions.** The container writes only to three host
directories, one mount each: the state directory (`/state`, the engine's
database; keep it on local storage), the download root (`/downloads`), and
the token directory (`/tokens`). Each must already exist and be owned by,
or writable by, the UID/GID the container runs as: `WAYFETCH_UID` and
`WAYFETCH_GID`, `1000:1000` by default.
Compose refuses to start if a path is missing, and never creates one.
Change ownership of these directories only, never recursively on a broader
folder:

```sh
export WAYFETCH_STATE=/srv/wayfetch/state WAYFETCH_TOKENS=/srv/wayfetch/tokens \
       WAYFETCH_DOWNLOADS=/srv/downloads
mkdir -p "$WAYFETCH_STATE" "$WAYFETCH_TOKENS"
sudo chown 1000:1000 "$WAYFETCH_STATE" "$WAYFETCH_TOKENS" "$WAYFETCH_DOWNLOADS"
sudo chmod 700 "$WAYFETCH_STATE" "$WAYFETCH_TOKENS"
```

Put these variables in a `.env` file next to `compose.yaml` to keep them.
Pass the same `-f deploy/compose.yaml` and variables to every command below.

**Initialize the root and start.** Initializing the root is a separate,
one-time step, so an unmounted drive's empty mount point is never mistaken
for the download root:

```sh
docker compose -f deploy/compose.yaml run --rm wayfetchd \
    init-root --state-dir /state /downloads
docker compose -f deploy/compose.yaml up -d
docker compose -f deploy/compose.yaml ps          # healthy once ready
```

On first start `wayfetchd` creates `$WAYFETCH_TOKENS/api-tokens`, owner-only,
holding one token with every permission; it never prints the token. Read it
on the host, or replace the file with your own tokens in the format above
(mode `600`, owned by the container's UID), then restart:

```sh
TOKEN=$(sudo awk '!/^#/ {print $1; exit}' "$WAYFETCH_TOKENS/api-tokens")
```

The health check runs `wayfetchd ready` inside the container. While the root
is uninitialized or its mount has gone, the container stays unhealthy and
downloads wait in `blocked` instead of writing anywhere else. Only one
engine can own a state directory: a second container on it exits with an
error. `docker compose stop` shuts down gracefully, and unfinished downloads
resume from their durable bytes when the container starts again, even after
`docker compose down` removes it.

**Using the API.**

```sh
API=http://127.0.0.1:7878
INSTANCE=$(curl -s -H "Authorization: Bearer $TOKEN" $API/v1/capabilities | jq -r .instance)
curl -s -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
    -d "{\"idempotency_key\":\"$(cat /proc/sys/kernel/random/uuid)\",\"target\":\"$INSTANCE\",\"items\":[\"https://example.com/file.zip\"]}" \
    $API/v1/inputs
curl -s -H "Authorization: Bearer $TOKEN" $API/v1/jobs
```

Downloads from addresses that are not public, such as a server on your LAN,
need `--allow-network`; add it, or `--ca-cert` for a private CA mounted
read-only, by overriding `command` with the image's command plus those
flags (see `CMD` in the `Dockerfile`).

**Networking.** Inside the container `wayfetchd` listens on `0.0.0.0:7878`,
so it logs a warning that the address is reachable beyond the container.
Compose publishes it only on the host's loopback, `127.0.0.1:7878`
(`WAYFETCH_PUBLISH` changes the host address and port). That is reachable
from the host itself, not from other computers. To use the API from another
computer, forward the port over SSH, which also encrypts it:

```sh
ssh -N -L 7878:127.0.0.1:7878 user@server      # then use http://127.0.0.1:7878
```

The API has no TLS of its own, so do not publish it on a LAN or public
address directly; put an authenticated TLS reverse proxy in front of it for
that.

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

The container tests need Docker with the Compose plugin, so they are ignored
by default. They build `wayfetch:local` with `docker build` first, or test
the image named by `WAYFETCH_IMAGE`:

```sh
cargo test --locked -p wayfetchd --test container -- --ignored
```

`scripts/check_dependency_boundaries.py` fails if the domain, protocol, engine,
net, host, or CLI crates gain a server, desktop, or UI-toolkit dependency (for
example Axum, Tauri, or GTK), if the engine-side crates depend on the host
crate, if the HTTP adapter or daemon gain a desktop dependency, if the domain
and protocol crates depend on the engine, or if the engine depends on the net
crate. It checks
every target with all features enabled, so optional dependencies count too. CI
also runs a repository hygiene check that rejects tracked agent-output
directories.

The HTTP tests start their own HTTP and HTTPS servers on `127.0.0.1`, with a
certificate generated per test; they need no network access. The crash tests
(`crates/net/tests/process_recovery.rs`, Unix only) run the engine in a child
process, a rerun of their own test binary, and kill it with `SIGKILL` at named
boundaries from the engine's test-only `failpoints` feature. The CLI tests
(`crates/cli/tests/session.rs`) drive the built `wayfetch` executable against
the same fixture servers, and the daemon tests (`apps/wayfetchd/tests/daemon.rs`)
drive the built `wayfetchd` over its HTTP API. The container tests
(`apps/wayfetchd/tests/container.rs`) run the image through
`deploy/compose.yaml` with a fixture server on the host's Docker bridge
address: a download resumes after the container is removed and recreated,
`docker compose stop` is graceful, a second container cannot take the state
directory, and a missing or uninitialized root is refused.

Use `cargo fmt --all` to apply formatting.

## License

Wayfetch Downloader is dual-licensed under [MIT](LICENSE-MIT) or
[Apache-2.0](LICENSE-APACHE), at your option.
