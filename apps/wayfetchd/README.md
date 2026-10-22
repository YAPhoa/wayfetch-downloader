# `wayfetchd` — Headless Server & HTTP/SSE API

`wayfetchd` runs `wayfetch-engine` headlessly and serves both the authenticated `/v1` HTTP/SSE API and the compiled-in web UI (`ui/src/`) from a single binary. It requires no external control portal, runtime Node.js server, or asset CDN.

## Building & Subcommands

Build from the repository root with `cargo build --locked -p wayfetchd` (`target/debug/wayfetchd`).

```sh
# Mark an existing directory as a destination root (once per root)
wayfetchd init-root --state-dir /srv/wayfetch/state /srv/downloads

# Run the daemon (listens on 127.0.0.1:7878 by default)
wayfetchd run --state-dir /srv/wayfetch/state --download-root /srv/downloads \
    --token-file /srv/wayfetch/api-tokens

# Or let the first start initialize the download root, as the container image
# does; once that has finished, it is never initialized again
wayfetchd run --state-dir /srv/wayfetch/state --download-root /srv/downloads \
    --token-file /srv/wayfetch/api-tokens --init-root-on-first-start

# Or load storage, network, download policy, plugin, and server settings from TOML
wayfetchd init-root --config /srv/wayfetch/wayfetch.toml /srv/downloads
wayfetchd run --config /srv/wayfetch/wayfetch.toml

# Container/service health probe (starts no engine, takes no lock, reads no token)
wayfetchd ready [--address 127.0.0.1:7878]

# Clear a forgotten browser owner account while the daemon is stopped
wayfetchd reset-owner --auth-dir /srv/wayfetch/auth
```

### Destination Roots

Downloads are saved in `--download-root` (`storage.download_root`) until the owner selects another default folder in Settings (`PUT /v1/settings`).

- **Adding roots**: Operators can initialize additional folders with `init-root` and pass `--root DIR` (repeatable) or `storage.roots`, or owners with `control` permission can add any server directory the daemon can open (except its state directory) via **Settings > Download folders > Add folder…** (`GET /v1/folders` and `POST /v1/roots`).
- **Removing & changing defaults**: Changing the default folder affects only downloads started afterwards. Any non-default root—including operator-configured ones—can be removed from the list (`POST /v1/roots/remove`), leaving its files on disk. If a job's assigned root is missing or unmounted, that job fails with `destination_unavailable` rather than silently writing elsewhere, and resumes there once the folder returns.

---

## Authentication & Token Permissions

`wayfetchd` listens on `127.0.0.1:7878` unless `--listen` (`server.listen`) specifies otherwise. Because `wayfetchd` does not terminate TLS itself, any listener beyond loopback belongs behind an authenticated TLS reverse proxy (see [`deploy/README.md`](../../deploy/README.md)).

Every `/v1/*` route (except `GET /v1/ready` and `/v1/auth/*`) requires either `Authorization: Bearer <TOKEN>` or an active browser session cookie.

The `--token-file` (`server.token_file`) must be readable only by its owner (`mode 600`) and holds one `TOKEN PERMISSION[,PERMISSION...]` entry per line. If the file does not exist on first start, `wayfetchd` creates it (`600`) with a single random token granted all permissions; tokens are never printed to logs.

| Permission | What it allows |
|---|---|
| `read` | Read capabilities, job lists/counts, single jobs, staged links, settings, and subscribe to `/v1/events` |
| `submit` | Submit direct downloads (`/v1/inputs`), collect staged links (`/v1/staging/links`), choose media formats (`choose_media`), and start staged links with explicit `"routing": "direct"` |
| `control` | Pause, resume, cancel, retry, rename, or remove jobs; move, remove, check, or analyze staged links; manage destination folders, engine settings, and plugins |
| `sources` | Read back raw submitted URLs (`/v1/jobs/{id}/source`, `/v1/staging/links/{id}/source`), which may carry sensitive signed query tokens |
| `accounts` | Inspect the credential vault, list/refresh/enable/disable/remove provider accounts, and start/cancel device-code sign-ins |
| `provider_downloads` | Route started links through a provider account (automatically or explicitly) and pass `"repeat_provider_operation": true` when retrying ambiguous provider failures |
| `delete_files` | Explicitly delete engine-owned downloaded files and partial staging data (`"action": "delete_files"`, alongside `control`) |

All 7 permissions are independent—none implies another.

---

## HTTP & SSE API Reference

All mutations require `"target"` (the `instance` UUID from `GET /v1/capabilities`) and `"idempotency_key"` (a fresh UUID per logical command). Retrying the same key and payload replays the original result; reusing a key with different parameters returns `idempotency_conflict`. Responses and errors use the engine's wire protocol types (`ProtocolVersion` `0.1`; before 1.0 a client must speak the same 0.x version as the engine).

| Route | Required Permission | Purpose & Payload |
|---|---|---|
| `GET /v1/ready` | *None* | Readiness probe: `200 {"ready": true}` or `503` with `reason`: `starting`, `shutting_down`, `engine_failed`, `engine_unavailable`, `download_root_unavailable`, or `check_timed_out` (2s cap; single-flight) |
| `GET /v1/capabilities` | `read` | Handshake: `instance`, `boot_epoch`, `protocol`, `head` event cursor, and `limits` |
| `POST /v1/inputs` | `submit` | Start direct downloads immediately: `{"idempotency_key", "target", "items": [URL...], "package"?}` |
| `GET /v1/jobs` | `read` | Page of jobs (`after`, `page_size`, `stage=a,b`, `package`) with `before` count and `as_of` cursor |
| `GET /v1/jobs/{id}` | `read` | Inspect one job and its `as_of` cursor |
| `GET /v1/job-counts` | `read` | Counts per stage (`{"stages": {"queued": 3, ...}, "as_of"}`): `stage=a,b`, `package` |
| `POST /v1/jobs/{id}/commands` | `control` (+ `delete_files` / `provider_downloads`) | `{"idempotency_key", "target", "action": "pause"\|"resume"\|"cancel"\|"retry"\|"remove"\|"delete_files"\|"rename"\|"cancel_extraction", "expected_revision"}`, plus `"name"` for `rename`, `"expected_generation"` (the job's `extraction.generation`) for `cancel_extraction`, and `"repeat_provider_operation": true` for `retry` when a provider response was lost |
| `POST /v1/jobs/{id}/extract` | `control` | Unpack a finished archive beside it: `{"idempotency_key", "target", "expected_revision", "expected_generation"?, "password"?}`, as a new run after the one `expected_generation` names (absent if it has none). The password is kept in memory for that archive's next attempt only, never stored or logged |
| `GET /v1/jobs/{id}/source` | `sources` | The raw URL the job was submitted with |
| `POST /v1/staging/links` | `submit` | Collect links without downloading: `{"idempotency_key", "target", "text", "base_url"?, "mode"?: "analyze"\|"supported_only", "destination"?: "root-N"}` (`text` up to 512 KiB of URLs or HTML) |
| `GET /v1/staging/links` | `read` | Page of collected links (`after`, `page_size`) with `total`, `before`, `checking`, and `as_of` |
| `POST /v1/staging/commands` | `submit` / `control` / `provider_downloads` | `{"idempotency_key", "target", "action": "start"\|"remove"\|"move"\|"check"\|"analyze"\|"choose_media", "links": [{"link", "revision"}...]}`, plus `"destination"` for `move`, `"media"` for `choose_media`, and optional `"routing": "direct"\|"automatic"` or `"account"` for `start` |
| `GET /v1/staging/links/{id}/source` | `sources` | A collected link exactly as entered |
| `GET /v1/settings` | `read` | Default destination, registered roots (`available`, `problem`, `added`), `max_active_downloads`, `download_speed_limit`, and `plugins` |
| `PUT /v1/settings` | `control` | `{"idempotency_key", "target", "expected_revision"}` with `"default_root": "root-N"`, `"max_active_downloads": {"set": N}\|"use_default"`, and/or `"download_speed_limit": {"set": "unlimited"}\|{"set": {"bytes_per_second": N}}\|"use_default"` |
| `GET /v1/folders` | `control` | Non-hidden subdirectories inside `path` (absolute; filesystem root if omitted; max 1,000) |
| `POST /v1/roots` | `control` | Initialize and register server directory `{"path"}` as a destination root |
| `POST /v1/roots/remove` | `control` | Stop offering non-default root `{"idempotency_key", "target", "expected_revision", "root"}` (files remain on disk) |
| `POST /v1/plugins/change` | `control` | Manage helper plugins (`yt-dlp`, `ffmpeg`): `{"idempotency_key", "target", "plugin", "change": "enable"\|"disable"\|"update"\|"remove"}` |
| `GET /v1/accounts` | `accounts` | Credential vault state, supported provider services, and redacted accounts |
| `POST /v1/accounts/authorizations` | `accounts` | Start device-code sign-in: `{"idempotency_key", "target", "service": "real-debrid", "alias"?}` or `"account": {"account", "revision"}` to re-authorize |
| `GET /v1/accounts/authorizations/{id}` | `accounts` | Poll a pending sign-in's code and approval state |
| `POST /v1/accounts/authorizations/{id}/cancel` | `accounts` | Cancel a pending device-code sign-in |
| `POST /v1/accounts/{id}/commands` | `accounts` | `{"idempotency_key", "target", "action": "enable"\|"disable"\|"remove", "expected_revision"}` |
| `POST /v1/accounts/{id}/refresh` | `accounts` | Check an account with its provider immediately |
| `GET /v1/events` | `read` | Server-Sent Events stream (`hello`, `event`, `progress`, `resync`) |
| `GET /v1/auth/session` | *Browser* | Session status, pending owner setup flag, owner username, and CSRF token |
| `POST /v1/auth/{setup,login,logout}` | *Browser* | Initial owner setup, password sign-in, and sign-out |

### Server Limits & Event Stream Semantics

- **Resource bounds**: Request bodies are capped at 1 MiB, concurrent HTTP connections at 64, and concurrent SSE streams at 16. Request headers must arrive within 10 seconds and bodies within 10 seconds thereafter. Disconnecting never cancels or interrupts a download.
- **Graceful drain**: `SIGINT` or `SIGTERM` closes SSE streams, gives in-flight HTTP requests up to 5 seconds to complete, and shuts the engine down cleanly so unfinished downloads resume on next start.
- **SSE replay & resync**: Durable events carry `INSTANCE:POSITION` as their SSE `id`. Reconnecting with `Last-Event-ID` (or `?after=INSTANCE:POSITION`) replays missed events from the engine's bounded ring. If a client falls too far behind or reconnects across a state reset, the stream emits a `resync` event and closes; the client then re-queries `/v1/jobs` and subscribes from the page's `as_of` cursor.

---

## Workflows & Engine Behavior

### Job Control (`rename`, `remove`, `delete_files`, `retry`)

- **`rename`**: Renames a completed job's file inside its destination folder. Never overwrites an existing file and refuses if the published file was modified outside Wayfetch.
- **`remove` vs. `delete_files`**: `remove` removes a completed, failed, or cancelled job from lists and counts while keeping its files on disk. `delete_files` (requires both `control` and `delete_files` permissions) deletes the engine-owned published file and partial staging data before removing the job record; if the file was modified externally, the job stays visible with an error so the user can either retry deletion or choose list-only removal.
- **`retry`**: Restarts a failed or stopped cancelled job under the same `JobId`, source link, and destination folder. Resumes from saved partial bytes when the server still offers the same file validator and accepts a range request; otherwise restarts from zero and records `restart_reason`. Failures with a scheduled cooldown deadline refuse early manual retry with `not_applicable` and `retry_at`.

### Link Staging & HTML Page Analysis

- Submitting via `POST /v1/staging/links` stores links across restarts without starting downloads. In raw HTML, only links to files and supported video pages are extracted (other links are counted). Invalid entries are retained with their `problem` description.
- **Background checks**: Collected links are probed in the background (at most 1 concurrent request per host and 2 overall) via `HEAD` (falling back to a 1-byte ranged `GET` when `HEAD` is refused) to populate filename hints, byte sizes, and `online` / `offline` status.
- **Page analysis (`analyze` mode)**: When a collected link is an HTML web page (and `mode` is `"analyze"`, the default), the engine reads up to 2 MiB of the page once and collects the file and video links found on it alongside the page entry (`found_on`), while counting and skipping links to other pages.
- **Untrusted page egress rule**: Links extracted from pasted HTML, or discovered on a web page that was not itself served from a local/allowed private address, may connect **only to public IP addresses** during both checks and downloads—regardless of `network.allow_networks`.

### Provider Accounts & Automatic Routing

- **Encrypted vault (`credential-key`)**: Provider tokens are encrypted with a 32-byte key (64 hex digits, `mode 600`) created automatically beside `--token-file` (or at `--credential-key FILE`) when the first account is added. The SQLite database never stores the key; keep `credential-key` backed up alongside the state directory. Once an account exists, `wayfetchd` never replaces a missing or mismatched key—accounts appear as `locked` until the key is restored or the accounts are removed.
- **Device-code authorization**: `POST /v1/accounts/authorizations` starts the provider's device flow, returning a user code and verification URL. No password or raw provider token ever passes through the Wayfetch API.
- **Routing (`automatic`, `direct`, or explicit `account`)**:
  - Starting staged links without `"account"` defaults to `"routing": "automatic"` (requires `submit` + `provider_downloads`): the engine matches each link's hostname (exact domain or dot-boundary subdomain) against the provider's cached supported-domain list (5-minute TTL, 15-second negative cache) and selects the lowest-ID usable enabled account, or uses `Direct` for known non-matching hosts.
  - If a host matches a provider whose accounts are disabled, expired, or locked—or if host support cannot be verified—the link remains staged in `/links` with a clear actionable reason rather than silently falling back to a direct download.
- **Real-Debrid specifics**:
  - Its own `real-debrid.com/d/<alphanumeric-id>` links are recognized locally and always require a usable Real-Debrid account (`real-debrid.download` CDN links remain direct-capable).
  - If a network failure loses the response after link generation, the engine checks the account's recent downloads list; if no match is found, the job fails with `operation_ambiguous` and requires `"repeat_provider_operation": true` on retry.

### Video Links, Archives & Plugins (`yt-dlp`, `ffmpeg` & 7-Zip)

- **YouTube resolution**: `youtube.com/watch`, `youtu.be`, `/shorts/`, `/live/`, and `/embed/` single-video links are inspected via `yt-dlp` (using Deno as the unprivileged JS runtime for player scripts, with caches under `STATE_DIR/cache/yt-dlp`). Users can choose **Best available**, a maximum resolution (e.g. `1080p`), or audio-only (`MP3` or `M4A`).
- **Engine-owned transfers**: `yt-dlp` only resolves metadata and stream URLs; `wayfetch-engine` downloads the streams itself so global speed limits, pause/resume, and egress policies apply, then invokes `ffmpeg` to mux video+audio or transcode to MP3. Without `ffmpeg`, only pre-muxed single-file formats and unconverted `M4A` audio are offered.
- **Archives**: a finished download named like an archive (`.zip`, `.7z`, `.rar`, `.tar`, `.tar.gz` and the other compressed tars, and multi-part sets such as `.part1.rar`, `.r00`, `.z01`, or `.7z.001`) is unpacked by the 7-Zip plugin (`7zz`, or the distributions' `7z`, 25.01 or newer) into a new folder beside it, named after the archive; an archive holding one file or folder gives just that. Nothing is replaced (a taken name becomes `Name (2)`) and the archive is always kept. A multi-part set waits until no other part is still downloading in that folder. Settings > Downloads turns unpacking after download off or on; a finished archive's row menu unpacks it, unpacks it again, or cancels unpacking. A protected archive shows Needs password, and the password goes to 7-Zip on standard input, never stored or logged. Listings naming files outside their folder, links, or devices, and output larger than listed or nearly filling the drive, are refused; only Wayfetch's private folder is removed. While an archive is unpacked, no part of its set can be renamed or deleted.
- **Confined 7-Zip**: it may read only the archive's parts and write only in Wayfetch's private folder, cannot open network connections, and may use at most 4 GiB of memory; its changes to permissions, owner, or times are skipped (Landlock, which needs Linux 6.2 or newer, seccomp, and resource limits). Cancelling stops 7-Zip and everything it started before the private folder is removed. Unpacked files get the time they were unpacked; with 7-Zip 26 or newer they keep their permissions. Where confinement cannot be enforced, such as on an older kernel, Settings > Plugins shows 7-Zip as not working and nothing is unpacked.
- **Where plugins are found**: at the paths `[plugins]` in `wayfetch.toml` names (`yt_dlp`, `ffmpeg`, `seven_zip`, and `yt_dlp_js_runtimes`, yt-dlp's `--js-runtimes` value), else in the desktop packages' bundled folder (`/usr/lib/wayfetch-downloader/plugins`), else on `PATH`; each is checked once at startup. The container image puts its copies on `PATH`.
- **Signed `yt-dlp` updates**: From **Settings > Plugins** (`POST /v1/plugins/change`), operators can enable/disable plugins, install or update `yt-dlp` into `STATE_DIR/plugins` (verified against pinned PGP key `AC0C BBE6 848D 6A87 3464 AF4E 57CF 6593 3B5A 7581`, SHA-256 checksums, and a runnable version check), or remove the installed update to roll back to the system/container binary. Set `updates = false` under `[plugins]` in `wayfetch.toml` to disable self-updates. ffmpeg and 7-Zip are not updated this way: neither publishes signed builds.

---

## Browser Access

Browser sign-in is disabled unless `wayfetchd run` is given `--auth-dir` (`server.auth_dir`, mode `700`) and at least one `--browser-origin` (`server.browser_origins`, which must be `https://`), or `--auth-dir` with `--dev-insecure-loopback` on a loopback listener, where the origins default to its address and `localhost`:

```sh
wayfetchd run --state-dir /srv/wayfetch/state --download-root /srv/downloads \
    --token-file /srv/wayfetch/api-tokens \
    --auth-dir /srv/wayfetch/auth --browser-origin https://wayfetch.example.lan
```

1. **First-time setup**: When no owner account exists, `wayfetchd` writes a single-use secret to `AUTH_DIR/setup-secret` (`mode 600`, valid for 15 minutes) and logs only the file path. Open the origin in a browser (or call `POST /v1/auth/setup`) to set the owner username and password (at least 12 characters, stored as an Argon2id hash).
2. **Headless setup via `curl` & `jq`** (password read without echo and passed via stdin, never CLI args):

```sh
ORIGIN=https://wayfetch.example.lan
SECRET=$(sudo cat /srv/wayfetch/auth/setup-secret)
read -rsp 'Owner password: ' PASSWORD; echo
(umask 077 && : > cookies.txt) && chmod 600 cookies.txt
SECRET=$SECRET PASSWORD=$PASSWORD \
    jq -n '{setup_secret: env.SECRET, username: "owner", password: env.PASSWORD}' |
    curl -sS --fail-with-body -c cookies.txt -H "Origin: $ORIGIN" \
        -H 'Content-Type: application/json' --data-binary @- "$ORIGIN/v1/auth/setup"
unset SECRET PASSWORD
```

3. **Signing in & CSRF**: `POST /v1/auth/login` sets an opaque `__Host-wayfetch-session` cookie (`Secure`, `HttpOnly`, `SameSite=Strict`, host-only) and returns `csrf_token`. Setup and login share a global rate limit of 5 attempts per minute (`429` with `Retry-After`). State-changing cookie requests must send `X-CSRF-Token`, carry a configured `Host` and `Origin`, and must not be marked `Sec-Fetch-Site: cross-site` or `same-site`.
4. **In-memory session lifecycle**: Sessions live in memory only (up to 32 active sessions, 12-hour idle timeout, 7-day maximum lifetime) and end on logout or daemon restart without interrupting running downloads.
