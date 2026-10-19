# `wayfetch-desktop` — Native Desktop Host

`wayfetch-desktop` embeds `wayfetch-engine` directly inside a native desktop process using [Tauri 2](https://tauri.app) and renders the shared web UI (`ui/src/`) in the system WebView (`ui/src/shell/desktop.html`).

## Architecture & Security Boundary

- **In-process engine, zero sockets**: There is no background daemon subprocess and no listening TCP port. The engine starts inside the desktop process before the window opens; if another process (`wayfetchd`, `wayfetch`, or another desktop window) already holds the state directory lock, `wayfetch-desktop` exits immediately without opening a window.
- **Constrained IPC surface**: The WebView communicates with the engine exclusively through the typed IPC commands in `src/ipc.rs` (`hello`, `query`, `submit`, `job_command`, `collect_links`, `links_command`, `update_settings`, `remove_root`, `change_plugin`, `authorize_account`, `cancel_authorization`, `account_command`, `refresh_account`, `choose_download_folder`, `open_file`, `reveal_file`, `subscribe`, `acknowledge`, `unsubscribe`).
- **Identical wire protocol**: IPC commands accept and return the exact same JSON DTOs and `ProtocolError` codes as the HTTP API, so idempotency keys, revision concurrency checks, and event resynchronization behave identically.
- **Capability lockdown**: `capabilities/main.json` grants only the application's IPC commands and **zero Tauri plugins**—the WebView has no direct filesystem, shell, database, or network access.

## Installing & Running (Linux)

Only Linux is built and tested so far. Because `wayfetch-desktop` requires GUI system libraries, it is excluded from Cargo's `default-members` so headless server builds never pull in WebKitGTK or GTK.

### From the `.deb` / `.rpm` package

Install `wayfetch-downloader_VERSION_amd64.deb` (`sudo apt install ./FILE.deb`) or `wayfetch-downloader-VERSION-1.x86_64.rpm` (`sudo dnf install ./FILE.rpm`) and start **Wayfetch Downloader** from the app menu. The packages depend on WebKitGTK 4.1 and GTK 3, and recommend Python 3.10 or later (which runs yt-dlp) and the distribution's `ffmpeg`.

- **First start, no setup**: started with no arguments, the app keeps its state in `$XDG_DATA_HOME/wayfetch-downloader` (`~/.local/share/wayfetch-downloader`) and downloads into the XDG download folder (`~/Downloads`). The download folder is initialized only on the first start, when the state directory does not exist yet; on later starts a missing folder is reported, never set up again, so an unmounted drive is not mistaken for an empty folder. **Settings > Download folders** chooses other folders.
- **Startup failures** (for example, another copy already owns the state directory) are shown in a message box as well as printed.
- **Bundled plugins**: yt-dlp (with yt-dlp-ejs) and Deno are installed in `/usr/lib/wayfetch-downloader/plugins`, built from the same pinned versions and hashes as the container image ([`deploy/plugins/`](../../deploy/plugins)). The app uses them before any copy on `PATH`, unless `[plugins]` in a `--config` file names others, and tells yt-dlp to run the bundled Deno. ffmpeg is found on `PATH`.
- The packages are not signed yet: check them against the release's `SHA256SUMS` and build attestation (`gh attestation verify FILE --repo YAPhoa/wayfetch-downloader`).

### Building the packages

[`deploy/desktop-release.sh`](../../deploy/desktop-release.sh) builds both packages and their `SHA256SUMS` in `target/release/bundle/` with the Tauri CLI. It needs the prerequisites below, Python 3 with pip, and `cargo install tauri-cli --version 2.12.3 --locked`. Build on the oldest distribution the packages should run on, since they need its glibc or newer. The release workflow ([`.github/workflows/desktop-release.yml`](../../.github/workflows/desktop-release.yml)) does this on Ubuntu 22.04 for a `vX.Y.Z` tag matching the workspace version and attaches the packages to a draft GitHub release.

### Prerequisites (Debian / Ubuntu)

```sh
sudo apt-get install -y libwebkit2gtk-4.1-dev libgtk-3-dev libsoup-3.0-dev librsvg2-dev
```

### Development build

The `init-root` and `run` subcommands name the directories explicitly, as for the daemon:

```sh
cargo build --locked -p wayfetch-desktop

# Initialize the download root once
target/debug/wayfetch-desktop init-root --state-dir /tmp/wf/state /tmp/wf/downloads

# Launch the desktop window (or pass --config FILE)
target/debug/wayfetch-desktop run --state-dir /tmp/wf/state --download-root /tmp/wf/downloads
```

## Lifecycle, Storage & Native Actions

- **Graceful window close**: Closing the window, pressing `Ctrl+C` (`SIGINT`), or sending `SIGTERM` shuts the engine down gracefully; unfinished downloads keep their checkpointed bytes and resume on the next launch. Closing does not yet ask first while downloads are active.
- **Bounded event subscriptions**: Reloading the page or losing an event stream never interrupts a transfer. The host queues at most 64 unacknowledged event items per subscription and allows at most 4 subscriptions per page; a subscription that stops acknowledging items for 15 seconds is dropped, and page reloads release previous subscriptions automatically.
- **Credential keyfile**: Provider account credentials (such as Real-Debrid) are encrypted with an owner-only keyfile created beside the state directory and named after it (`/tmp/wf/state.credential-key` for `/tmp/wf/state`, `~/.local/share/wayfetch-downloader.credential-key` for an app-menu start), or at `--credential-key FILE`. Keep the `.credential-key` file alongside the state directory when moving state.
- **Native folder dialog & file opening**:
  - Clicking **Choose folder…** in **Settings > Download folders** opens the OS folder picker from the Rust host (`src/picker.rs`); the WebView never sends a filesystem path. The chosen directory is initialized and registered as a destination root across restarts.
  - From a completed download's row menu, **Open file** and **Open folder** launch the system default handler (`xdg-open`) only after the engine verifies that the published artifact still exists and matches what it wrote.

## Verification & Window Tests

The integration suite in `tests/shell.rs` drives the real desktop window via `WebKitWebDriver` (`webkit2gtk-driver`) under `Xvfb` (`xvfb`). Because windows on the same X display share keyboard focus, run the ignored window tests serially with `--test-threads=1`:

```sh
# Lint & unit tests
cargo clippy --locked -p wayfetch-desktop --all-targets -- -D warnings
cargo test --locked -p wayfetch-desktop

# Real-window WebKitWebDriver integration suite (requires webkit2gtk-driver and xvfb)
xvfb-run -a cargo test --locked -p wayfetch-desktop --features scripted-picker --test shell -- --ignored --test-threads=1
```
