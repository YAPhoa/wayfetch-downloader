# Deployment, Configuration & Reverse Proxy Guide

This directory holds the container deployment ([`compose.yaml`](compose.yaml)) and annotated configuration template ([`wayfetch.example.toml`](wayfetch.example.toml)) for running `wayfetchd` on a Linux server or NAS.

## Configuration File (`wayfetch.toml`)

`wayfetchd`, `wayfetch`, and `wayfetch-desktop` all accept `--config FILE`. See [`wayfetch.example.toml`](wayfetch.example.toml) for every key.

```sh
wayfetchd init-root --config /srv/wayfetch/wayfetch.toml /srv/downloads
wayfetchd run --config /srv/wayfetch/wayfetch.toml
```

### Rules & Precedence

- **Explicit & startup-only**: Nothing reads a configuration file unless `--config FILE` is passed, and changes require a restart.
- **CLI override behavior**: Single-valued CLI flags replace values from the file; repeatable flags (`--root`, `--allow-network`, `--ca-cert`, `--browser-origin`) append to the file's lists. Relative paths inside the file resolve against the file's own directory.
- **No secrets in TOML**: The file holds no credentials or tokens. Unknown keys are rejected, and parse errors report the line and column without echoing file contents.
- **Host boundary**: Only `wayfetchd` accepts the `[server]` section; `wayfetch` and `wayfetch-desktop` reject it so server settings are never silently ignored.

### Download Policies (`[downloads]`)

- **`[downloads.max_active]`**: Sets how many downloads transfer concurrently across the engine:
  - `default` (`4` unless set), `min` and `max` bounds (`1` to `64` at most) for what users may choose in Settings, and `locked = true` to fix the setting at `default`.
  - User choices saved in Settings are stored in the engine database (never written back to the TOML file) and take effect immediately without stopping running transfers.
- **`[downloads.speed_limit]`**: Caps the combined download body rate across all active transfers in whole bytes per second (`1024` = `1 KiB/s`, `1048576` = `1 MiB/s`):
  - `default` is `"unlimited"` (built-in default) or an integer; `min` and `max` bound user choices (`1 KiB/s` to `1 TiB/s`); `locked = true` fixes the setting at `default`.
  - Setting `max` also forbids Unlimited (requiring a finite `default`, and clamping any previously saved Unlimited preference to `max`).
  - Changes in Settings apply immediately to active downloads without restarting them. The cap covers download bodies (allowing a short burst up to 250 ms or 16 MiB) and does not throttle control/API traffic.

---

## Container Deployment (`Dockerfile` & `compose.yaml`)

The root [`Dockerfile`](../Dockerfile) builds `wayfetchd` with the toolchain pinned in `rust-toolchain.toml` and packages it on a distroless Debian 12 Python 3.11 base (`gcr.io/distroless/python3-debian12:nonroot`, containing no shell or package manager) alongside pinned, SHA-256-verified copies of the following, built by [`plugins/build.sh`](plugins/build.sh) from the pins in [`plugins/`](plugins) that the desktop packages share:
- `yt-dlp` and `yt-dlp-ejs` (packaged as a standalone Python zipapp)
- `deno` (sandboxed JS runtime for YouTube player scripts)
- `static-ffmpeg` (for muxing video/audio streams and MP3 conversion)
- 7-Zip's official static `7zzs` (for unpacking downloaded archives, pinned in the `Dockerfile`)

[`compose.yaml`](compose.yaml) runs the image with least privilege:
- Configurable `WAYFETCH_UID:WAYFETCH_GID` (`1000:1000` by default, the first user on most Linux systems)
- `read_only: true` root filesystem with a `16m` `/tmp` tmpfs
- `cap_drop: [ALL]`, `no-new-privileges:true`, `pids_limit: 128`, and `init: true`
- `stop_grace_period: 45s` (covering the 5s HTTP API drain plus 30s engine task shutdown before Docker sends `SIGKILL`)

### 1. Build the Image & Prepare Host Directories

The container writes only to three explicit bind mounts (`/state`, `/downloads`, `/tokens`) with `create_host_path: false`—Compose refuses to start if a host directory is missing, preventing accidental writes to an unmounted dataset or container overlay.

```sh
# Build from the repository root
docker build -t wayfetch:local .

# Prepare the three host directories (change ownership only on these directories,
# never recursively across an entire media folder)
export WAYFETCH_STATE=/srv/wayfetch/state \
       WAYFETCH_TOKENS=/srv/wayfetch/tokens \
       WAYFETCH_DOWNLOADS=/srv/downloads
mkdir -p "$WAYFETCH_STATE" "$WAYFETCH_TOKENS"
sudo chown 1000:1000 "$WAYFETCH_STATE" "$WAYFETCH_TOKENS" "$WAYFETCH_DOWNLOADS"
sudo chmod 700 "$WAYFETCH_STATE" "$WAYFETCH_TOKENS"
```

Save `WAYFETCH_STATE`, `WAYFETCH_TOKENS`, and `WAYFETCH_DOWNLOADS` in a `.env` file beside `deploy/compose.yaml` so you do not need to re-export them.

### 2. Initialize the Download Root & Start

Initializing the download root is an explicit one-time step so an unmounted drive or network share is never mistaken for an empty download directory:

```sh
docker compose -f deploy/compose.yaml run --rm wayfetchd \
    init-root --state-dir /state /downloads
docker compose -f deploy/compose.yaml up -d
docker compose -f deploy/compose.yaml ps          # becomes healthy once ready
```

On first start, `wayfetchd` creates `$WAYFETCH_TOKENS/api-tokens` (`mode 600`) with a single all-permissions token. Read it from the host to use the API:

```sh
TOKEN=$(sudo awk '!/^#/ {print $1; exit}' "$WAYFETCH_TOKENS/api-tokens")
API=http://127.0.0.1:7878
INSTANCE=$(curl -s -H "Authorization: Bearer $TOKEN" "$API/v1/capabilities" | jq -r .instance)

curl -s -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
    -d "{\"idempotency_key\":\"$(cat /proc/sys/kernel/random/uuid)\",\"target\":\"$INSTANCE\",\"items\":[\"https://example.com/file.zip\"]}" \
    "$API/v1/inputs"
curl -s -H "Authorization: Bearer $TOKEN" "$API/v1/jobs"
```

When the first provider account (such as Real-Debrid) is added, `wayfetchd` creates `$WAYFETCH_TOKENS/credential-key` (`mode 600`) beside the token file. Back up `credential-key` alongside `$WAYFETCH_STATE`.

---

## Networking & Reverse Proxy Setup

Inside the container `wayfetchd` listens on `0.0.0.0:7878`, while `compose.yaml` publishes it **only on the host's loopback** (`127.0.0.1:7878`, configurable via `WAYFETCH_PUBLISH`).

### Remote CLI/API Access over SSH

To reach the loopback port from another machine without setting up TLS, forward the port over SSH:

```sh
ssh -N -L 7878:127.0.0.1:7878 user@server
# Then open or query http://127.0.0.1:7878 locally
```

### Browser Access Behind a TLS Reverse Proxy

Browsers require HTTPS for `__Host-wayfetch-session` cookies. Place a TLS reverse proxy in front of `127.0.0.1:7878` and enable `--auth-dir` and `--browser-origin` in `compose.yaml`:

```yaml
    command: ["run", "--state-dir", "/state", "--download-root", "/downloads",
              "--token-file", "/tokens/api-tokens", "--listen", "0.0.0.0:7878",
              "--auth-dir", "/tokens/auth",
              "--browser-origin", "https://wayfetch.example.lan"]
```

Read the initial one-time setup secret from `$WAYFETCH_TOKENS/auth/setup-secret` within 15 minutes of startup. To reset a forgotten owner password later:

```sh
docker compose -f deploy/compose.yaml stop
docker compose -f deploy/compose.yaml run --rm wayfetchd reset-owner --auth-dir /tokens/auth
docker compose -f deploy/compose.yaml start
```

**Reverse proxy requirements:**
1. Terminate TLS for the configured origin and forward to `127.0.0.1:7878`.
2. Preserve the original `Host` header (to prevent DNS rebinding) and pass `Origin`, `Cookie`, `X-CSRF-Token`, and `Sec-Fetch-*` unchanged.
3. Do not inject CORS headers.
4. Disable response buffering and use a long read timeout so `/v1/events` Server-Sent Events stream cleanly.

**Caddy** (satisfies all requirements by default):

```text
wayfetch.example.lan {
    reverse_proxy 127.0.0.1:7878
}
```

**Nginx** (inside your TLS `server` block):

```nginx
location / {
    proxy_pass http://127.0.0.1:7878;
    proxy_set_header Host $host;
    proxy_http_version 1.1;
    proxy_buffering off;          # required for /v1/events SSE
    proxy_read_timeout 1h;
}
```

---

## Container Integration Tests

The container test suite (`apps/wayfetchd/tests/container.rs`) builds `wayfetch:local` (or tests the image named by `WAYFETCH_IMAGE`) and exercises it through `deploy/compose.yaml` against a local fixture server on the Docker bridge network—verifying UI serving, download resume across container removal/recreation, graceful `docker compose stop`, single-instance state lock enforcement, and uninitialized-root refusal:

```sh
cargo test --locked -p wayfetchd --test container -- --ignored
```
