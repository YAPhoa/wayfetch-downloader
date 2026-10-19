# `ui/` — Shared Web UI

The Wayfetch web UI (`ui/src/`) is shared by both `wayfetchd` (served at `/` over HTTP/SSE) and `wayfetch-desktop` (loaded from `ui/src/shell/desktop.html` over Tauri IPC).

It is written in plain ES modules with **no bundler or runtime build step**—`wayfetchd` and `wayfetch-engine` compile and run without Node.js or npm installed. `ui/package.json` contains only development tools: TypeScript (for JSDoc type-checking) and Playwright (for real-browser E2E tests).

## Structure

```text
ui/
├── package.json                # Dev-only checks: tsc, node --test, playwright
├── tsconfig.json               # Strict JSDoc typechecking against generated wire types
├── src/
│   ├── index.html / main.js    # Browser shell (owner setup, sign-in, session recovery)
│   ├── shell/desktop.html      # Desktop WebView entry point (no sign-in needed)
│   ├── style.css / theme.js    # Light/dark theme, density, and the dense table layout
│   ├── fonts/                  # Bundled IBM Plex Sans WOFF2 (SIL OFL 1.1)
│   ├── data/services.json      # Searchable provider/hoster catalogue
│   ├── client/                 # Transport-neutral WayfetchClient contract, JobFeed, sorting
│   │   └── generated/          # TypeScript wire definitions exported from Rust by ts-rs
│   ├── transports/             # HttpClient (REST + SSE) and IpcClient (Tauri channels)
│   └── components/             # Downloads, Links, Add Links, Settings, Plugins, Accounts, Dialogs
└── tests/
    ├── unit/                   # Fast Node.js unit tests
    └── e2e/                    # Chromium E2E suites against wayfetchd (browser, media, toolbar)
```

## Views & Workflows

- **Downloads (`/`, `/jobs/job-N`)**:
  - Dense desktop table showing 50 jobs per page in submission order with keyset pagination (`before` position counts), state filter sidebar with live per-stage job counts, name filter, and a bottom details inspector.
  - Follows the event stream (`JobFeed`) and reconnects automatically using `Last-Event-ID`, or re-reads `/v1/jobs` when the engine sends `resync`.
  - Displays live 1-second transfer progress samples (bytes received, total size, 5-second rolling speed, and ETA) alongside colour-coded state badges and progress bars.
  - Supports single and bulk actions via the toolbar or row context menu (`…` button, right-click, `Shift+F10`, or `Menu` key): **Pause**, **Resume**, **Cancel**, **Retry** (prompting for confirmation when `operation_ambiguous` would repeat a provider request), **Rename** (within the destination folder), **Copy link** (requires `sources` permission; query parameters are hidden in the inspector by default), **Remove from list** (keeps files on disk by default), and **Delete files** (requires `delete_files` permission).
- **Add Links (`/add`) & Links (`/links`)**:
  - Collects pasted URLs or raw HTML into `/links` for review before starting downloads.
  - Background checks update each link with **Online**, **Offline** (with reason), **Page**, **Unknown**, or **Invalid** status, server-reported filename hints, and sizes. Web pages collected in `analyze` mode list extracted file and video links beside the source page (`found_on`).
  - Sortable by name, host, size, status, folder, or time added (persisted per browser).
  - Recognizes YouTube links (`Checking video`), displays the resolved video title, and offers a **Format** picker: **Best available**, height limits (such as `1080p`), or audio-only (`MP3` / `M4A`).
  - **Start selected** routes links automatically in the engine (`StartRouting::Automatic`) and switches to Downloads once all selected links start, or stays on `/links` to display per-link refusal reasons. Individual rows also offer **Download via…** (`Direct` or a specific enabled provider account).
- **Settings (`/settings/*`)**:
  - **Downloads**: Configure `max_active_downloads` and `download_speed_limit` (`KiB/s`, `MiB/s`, or `Unlimited`) within operator-defined policy ranges (`min`, `max`, `locked`), with clear conflict explanations.
  - **Download folders**: Switch the default destination root, remove non-default roots, or add new folders (browsing server paths via `GET /v1/folders` in a browser, or opening the native OS folder dialog in the desktop app).
  - **Accounts & services**: Searchable catalogue of file hosts, cloud storage, and multi-host services (searchable by name, domain, or pasted URL). Supports **Real-Debrid** device-code sign-in, live status/plan/expiry checks, enable/disable toggles, and removal.
  - **Plugins**: Shows detected `yt-dlp` and `ffmpeg` versions and status, with actions to enable/disable plugins, install or update `yt-dlp` from official PGP-signed GitHub releases, or remove an installed update to roll back.
  - **Display**: System/Light/Dark theme toggle and Compact/Comfortable row density (stored locally per browser).

## Security & Asset Serving

- **Content Security Policy & Trusted Types**: `wayfetchd` serves the UI with a strict CSP that permits scripts, styles, fonts, and connections only from `'self'`, forbids framing (`frame-ancestors 'none'`), disallow inline styles, and enforces `require-trusted-types-for 'script'`. All DOM nodes are built through `h()` (`src/components/dom.js`) so untrusted filenames and diagnostic messages are always rendered as plain text.
- **Zero external requests & cache busting**: All assets (including the IBM Plex Sans WOFF2 font files in `src/fonts/`) are compiled into the binary. `/` is revalidated on every load, while versioned assets under `/assets/VERSION/` (where `VERSION` is a content digest of the bundle) are cached immutable.

## Type Checking, Wire Types (`ts-rs`) & Testing

The TypeScript declarations in `src/client/generated/` are exported directly from the Rust protocol and API crates via [`ts-rs`](https://github.com/Aleph-Alpha/ts-rs) behind each crate's `ts` feature.

```sh
# 1. Regenerate TypeScript wire definitions from Rust (from repository root)
cargo test --locked --workspace --exclude wayfetch-desktop --all-features

# 2. Install dev tools (requires Node.js >= 22)
cd ui && npm ci

# 3. Run JSDoc TypeScript checks and unit tests
npm run check
npm test

# 4. Run real-browser Playwright E2E tests against wayfetchd
cargo build --locked -p wayfetchd
npx playwright install chromium   # first time only
npm run e2e
```

CI deletes `ui/src/client/generated/` before running `cargo test --all-features` and fails if `git diff` shows uncommitted type changes.
