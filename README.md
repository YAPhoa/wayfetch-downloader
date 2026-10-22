<picture>
  <source media="(prefers-color-scheme: dark)" srcset="ui/src/brand/logo-dark.svg">
  <img src="ui/src/brand/logo-light.svg" alt="Wayfetch" height="64">
</picture>

# Wayfetch Downloader

A Rust download manager built around one reusable download engine (`wayfetch-engine`) and three thin application hosts:

- **`wayfetch`** (`crates/cli`) — Foreground command-line session driven over standard input.
- **`wayfetchd`** ([`apps/wayfetchd/`](apps/wayfetchd/README.md)) — Headless daemon serving an authenticated HTTP/SSE API and a bundled, zero-CDN web UI.
- **`wayfetch-desktop`** ([`apps/wayfetch-desktop/`](apps/wayfetch-desktop/README.md)) — Native desktop application (Tauri 2) embedding the engine in-process over constrained IPC with no daemon subprocess or listening socket.

## Version

The current release is **0.1.2**. The workspace crates, desktop packages, and release tag (`v0.1.2`) share this version. The engine protocol is `0.3` and the state schema is version 3. Existing 0.1.1 databases upgrade automatically; back up the state directory before upgrading, since older binaries cannot reopen the migrated database. Before 1.0, clients must match the engine's 0.x protocol version. State directories from development builds before 0.1 are refused rather than upgraded.

## Core Capabilities

- **Durable, crash-safe transfers**: Locked SQLite state directory (`cap-std` confined roots), 8 MiB durable checkpoints, conditional HTTP resume (`ETag` / `Last-Modified` / `206 Partial Content`), atomic publication into verified destination roots, and generation-checked cancellation.
- **Link staging and page analysis**: Collect links or raw HTML into a persistent staging queue (`/links`) without starting transfers immediately. Background checks (`HEAD` or 1-byte `GET`, bounded per host) inspect availability, filenames, and sizes; `analyze` mode reads web pages (up to 2 MiB) to extract embedded file and video links.
- **Provider accounts and automatic routing**: OAuth-style device-code sign-in for multi-host providers (**Real-Debrid**) with owner-only keyfile encryption (`credential-key`). Collected links route automatically by matching hostnames against provider-supported domains, or explicitly via Direct / account overrides.
- **YouTube video and audio downloads**: Resolves video titles and formats via `yt-dlp`, downloads streams through the engine's own rate-limited, resumable transfer pipeline, and muxes or converts audio (`MP3` / `M4A`) with `ffmpeg`. Supports PGP-verified `yt-dlp` updates in Settings.
- **Strict network and security boundaries**: Public-IP-only egress policy re-checked on every redirect (with explicit `--allow-network` CIDR and `--ca-cert` exceptions), no automatic HTTP decompression, non-replayable transfer leases, scoped API bearer tokens, and Argon2id browser owner sessions with `SameSite=Strict` cookies, `X-CSRF-Token`, and Trusted Types CSP.

## Repository Structure & Documentation Map

| Guide | What it covers |
|---|---|
| [`apps/wayfetchd/README.md`](apps/wayfetchd/README.md) | `wayfetchd` daemon CLI, bearer token scopes, complete `/v1/*` HTTP & SSE API reference, staging/page analysis, Real-Debrid accounts, `yt-dlp`/`ffmpeg` plugins, and browser authentication |
| [`deploy/README.md`](deploy/README.md) | `wayfetch.toml` configuration reference, Distroless Docker image & Compose deployment (`568:568` TrueNAS SCALE defaults), SSH port forwarding, and Caddy/Nginx TLS reverse proxy setup |
| [`apps/wayfetch-desktop/README.md`](apps/wayfetch-desktop/README.md) | Native Linux desktop host (Tauri 2), `.deb`/`.rpm` packages and the release workflow, first start from the app menu, in-process IPC security boundary, WebKitGTK 4.1 dependencies, native folder picker, and `xvfb-run` window tests |
| [`ui/README.md`](ui/README.md) | Zero-build ES module web UI, Downloads/Links/Settings workflows, Content Security Policy & Trusted Types, `ts-rs` generated wire types, and Playwright E2E tests |
| [`docs/architecture.md`](docs/architecture.md) | Canonical architecture specification and Architecture Decision Register (ADR) |
| [`CONTRIBUTING.md`](CONTRIBUTING.md) & [`AGENTS.md`](AGENTS.md) | Contribution workflow, verification standards, and repository hygiene rules |

```text
wayfetch-downloader/
├── Cargo.toml / Cargo.lock      # Workspace manifest and locked dependencies
├── rust-toolchain.toml          # Pinned Rust toolchain (1.97)
├── Dockerfile                   # Distroless wayfetchd server image
├── deploy/                      # Docker Compose, wayfetch.example.toml, deployment guide
│   ├── plugins/                 # yt-dlp & deno pins, shared by the image and desktop packages
│   └── desktop-release.sh       # Builds the Linux desktop .deb and .rpm
├── apps/
│   ├── wayfetchd/               # Headless server and bundled UI host
│   └── wayfetch-desktop/        # Desktop host embedding the engine via Tauri 2 IPC
├── crates/
│   ├── domain/                  # IDs, job states, source identity, failure scopes
│   ├── protocol/                # Transport-neutral commands, queries, errors, events
│   ├── engine/                  # SQLite store, safe roots, checkpoints, transfer scheduler
│   ├── net/                     # HTTP(S) transport, TLS, redirect egress policy, leases
│   ├── adapters/                # Real-Debrid, YouTube, yt-dlp, ffmpeg integrations
│   ├── host/                    # Shared CLI/config/runtime/signal composition
│   ├── cli/                     # Foreground `wayfetch` CLI host
│   └── api/                     # HTTP/1.1 & SSE server, bearer and browser auth
├── ui/                          # Shared ES-module web UI, JSDoc types, Playwright suite
└── scripts/                     # CI crate dependency-boundary checks
```

## Using the `wayfetch` CLI

`wayfetch` runs the engine in the foreground and reads commands from standard input. Build it with `cargo build --locked -p wayfetch-cli` (`target/debug/wayfetch`).

```sh
# 1. Mark an existing directory as a download root (done once per root so an
#    unmounted filesystem is never mistaken for an empty directory). Other
#    Wayfetch profiles may use the same folder; each keeps its own marker and
#    temporary files under .wayfetch/ there.
wayfetch init-root --state-dir /srv/wayfetch/state /srv/downloads

# 2. Start a foreground session (or pass --config /srv/wayfetch/wayfetch.toml)
wayfetch run --state-dir /srv/wayfetch/state --download-root /srv/downloads
```

Inside a session (`JOB` accepts `job-1` or `1`):

| Command | Action |
|---|---|
| `URL...` or `add URL...` | Queue one or more `http(s)://` URLs for immediate download |
| `list` | List all jobs with their stage and durable checkpointed bytes |
| `inspect JOB` | Show full durable state for a single job |
| `pause JOB` / `resume JOB` | Pause (keeping partial data) or resume a job |
| `cancel JOB` | Stop a job while retaining partial data |
| `retry JOB` | Restart a failed or cancelled job, resuming saved bytes when the server's file validator still matches |
| `help` | Print session command summary |
| `quit` / `exit` | Shut down gracefully (`EOF`, `Ctrl+C`, and `SIGTERM` do the same) |

**Session invariants:**
- **Graceful shutdown**: `quit`, end of input, `Ctrl+C`, or `SIGTERM` never cancels active downloads; unfinished jobs resume from their last 8 MiB durable checkpoint on the next start.
- **Single-instance ownership**: Only one process (`wayfetch`, `wayfetchd`, or `wayfetch-desktop`) may own a state directory at a time; a second invocation is refused.
- **Privacy & bounded I/O**: URLs are read only from `stdin` (never exposed in process listings) and are never echoed in output or error messages. Input lines over 16 KiB are discarded. While transfers run, the event stream emits live progress samples (`<job> <bytes> at <rate> B/s`), whereas `list` and `inspect` report durable 8 MiB checkpointed bytes. Pass `--json` to emit one JSON object per line for scripts; if the stdout reader stalls, lines beyond a small bounded queue are dropped and reported later so output never blocks transfers or shutdown.
- **Network policy**: Only public IP addresses are reachable by default and TLS certificates are always verified against platform roots. Pass `--allow-network CIDR` and `--ca-cert FILE` (PEM/DER) for private LAN servers or internal CAs.

## Running `wayfetchd` and the Web UI

Build with `cargo build --locked -p wayfetchd` (`target/debug/wayfetchd`). To try the server and bundled web UI on a single machine over loopback:

```sh
wayfetchd init-root --state-dir /tmp/wf/state /tmp/wf/downloads
wayfetchd run --state-dir /tmp/wf/state --download-root /tmp/wf/downloads \
    --token-file /tmp/wf/api-tokens --auth-dir /tmp/wf/auth --dev-insecure-loopback
# Open http://127.0.0.1:7878 and enter the secret from /tmp/wf/auth/setup-secret
```

See [**`apps/wayfetchd/README.md`**](apps/wayfetchd/README.md) for the complete HTTP/SSE API reference, bearer token permissions, link staging, Real-Debrid accounts, `yt-dlp`/`ffmpeg` plugins, and production HTTPS browser authentication.

## Running in a container

The root `Dockerfile` packages `wayfetchd` alongside pinned, hash-verified copies of `yt-dlp`, `yt-dlp-ejs`, `deno`, and static `ffmpeg` on a distroless Debian 12 base (no shell or package manager). [`deploy/compose.yaml`](deploy/compose.yaml) runs it with a read-only root filesystem, `cap_drop: [ALL]`, `no-new-privileges`, a 128-process limit, and TrueNAS SCALE's `568:568` default UID/GID:

It needs two host folders: `WAYFETCH_DATA` for Wayfetch's own data and `WAYFETCH_DOWNLOADS` for downloads, set in `deploy/.env` (copy [`deploy/.env.example`](deploy/.env.example)):

```sh
sudo mkdir -p /mnt/tank/apps/wayfetch
sudo chown 568:568 /mnt/tank/apps/wayfetch /mnt/tank/downloads
sudo chmod 700 /mnt/tank/apps/wayfetch
cp deploy/.env.example deploy/.env      # then set the two folders

docker compose -f deploy/compose.yaml up -d
```

Compose pulls `ghcr.io/yaphoa/wayfetch-downloader:v0.1.2` (Linux x86-64).
Set `WAYFETCH_IMAGE` in `deploy/.env` to select another compatible version
or an immutable digest. Existing 0.1.0 container installations must follow
the [storage migration steps](deploy/README.md#upgrading-from-010).

See [**`deploy/README.md`**](deploy/README.md) for full container operations, `wayfetch.toml` configuration policies, SSH port forwarding, and Caddy/Nginx reverse-proxy configurations.

## Running the desktop app

`wayfetch-desktop` embeds `wayfetch-engine` in-process and renders the shared web UI in a system WebView via Tauri 2 (Linux with WebKitGTK 4.1). Installed from its `.deb` or `.rpm` package (built by [`deploy/desktop-release.sh`](deploy/desktop-release.sh)), it starts from the app menu with no setup, keeping its state in `~/.local/share/wayfetch-downloader` and downloading into `~/Downloads`. For development:

```sh
cargo build --locked -p wayfetch-desktop
target/debug/wayfetch-desktop init-root --state-dir /tmp/wf/state /tmp/wf/downloads
target/debug/wayfetch-desktop run --state-dir /tmp/wf/state --download-root /tmp/wf/downloads
```

See [**`apps/wayfetch-desktop/README.md`**](apps/wayfetch-desktop/README.md) for the packages and release workflow, system package prerequisites, IPC security boundaries, and WebKitWebDriver window tests.

## Working on the project

Read [CONTRIBUTING.md](CONTRIBUTING.md), [AGENTS.md](AGENTS.md), and the relevant sections of [docs/architecture.md](docs/architecture.md) before making changes.

Building and testing the engine, CLI, and `wayfetchd` requires only `rustup` (which installs the toolchain pinned in `rust-toolchain.toml`), Python 3, and a C compiler (for bundled SQLite)—no Node.js or desktop libraries are needed. CI runs these core checks on every push and pull request:

```sh
cargo fmt --all -- --check
cargo clippy --locked --workspace --exclude wayfetch-desktop --all-targets --all-features -- -D warnings
cargo test --locked --workspace --exclude wayfetch-desktop --all-features  # also regenerates UI ts-rs types
cargo build --locked -p wayfetch-engine
python3 scripts/check_dependency_boundaries.py
python3 -m unittest discover -s scripts
```

`scripts/check_dependency_boundaries.py` enforces strict crate layering across all targets and feature flags: `domain`, `protocol`, `engine`, `net`, `host`, and `cli` may never depend on server (`axum`, `rust-embed`), desktop (`tauri`, `gtk`, `webkit2gtk`), or host-binary crates; `engine` never depends on `net` or `host`; `adapters` hold no HTTP client of their own; and `wayfetchd` and `wayfetch-desktop` never depend on each other.

For component-specific test suites (Playwright browser E2E, WebKitWebDriver desktop window tests, and Docker Compose container tests), see [`ui/README.md`](ui/README.md), [`apps/wayfetch-desktop/README.md`](apps/wayfetch-desktop/README.md), and [`deploy/README.md`](deploy/README.md).

## License

Wayfetch Downloader is dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.

- The web UI bundles the **IBM Plex Sans** typeface (`ui/src/fonts/`), licensed under the [SIL Open Font License 1.1](ui/src/fonts/OFL.txt).
- The container image also packages unmodified, standalone helper programs: [yt-dlp](https://github.com/yt-dlp/yt-dlp) and [yt-dlp-ejs](https://github.com/yt-dlp/ejs) (Unlicense; MIT/ISC bundled JavaScript), [Deno](https://github.com/denoland/deno) (MIT), and the [static-ffmpeg](https://github.com/wader/static-ffmpeg) build of [FFmpeg](https://ffmpeg.org/legal.html) (GPL-3.0 or later; source linked from that project), and [7-Zip](https://7-zip.org/license.txt)'s official static build (GNU LGPL-2.1 or later, with BSD-licensed parts and the unRAR license restriction for its RAR code). Their license texts are installed under `/usr/local/share/licenses` in the image (`ffmpeg -L` prints FFmpeg's).
- The desktop `.deb` and `.rpm` packages ship the same yt-dlp, yt-dlp-ejs, and Deno builds, with their license texts in `/usr/lib/wayfetch-downloader/plugins/licenses`; they do not ship FFmpeg, which comes from the distribution's own package.
