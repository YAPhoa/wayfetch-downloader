# Deployment, Configuration & Reverse Proxy Guide

This directory holds the container deployment ([`compose.yaml`](compose.yaml)) and annotated configuration template ([`wayfetch.example.toml`](wayfetch.example.toml)) for running `wayfetchd` on a NAS (such as TrueNAS SCALE) or Linux server.

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
- Configurable `WAYFETCH_UID:WAYFETCH_GID` (`568:568` by default, matching the TrueNAS SCALE `apps` user)
- `read_only: true` root filesystem with a `16m` `/tmp` tmpfs
- `cap_drop: [ALL]`, `no-new-privileges:true`, `pids_limit: 128`, and `init: true`
- `stop_grace_period: 45s` (covering the 5s HTTP API drain plus 30s engine task shutdown before Docker sends `SIGKILL`)

### Quick start

Compose pulls the published image, `ghcr.io/yaphoa/wayfetch-downloader:v0.1.0` (Linux x86-64), and builds it from this repository only if it cannot. The image includes the web UI; no separate frontend container is needed. To choose another version, or pin an immutable `ghcr.io/yaphoa/wayfetch-downloader@sha256:…` digest, set `WAYFETCH_IMAGE` in `deploy/.env`; to always build locally, set `WAYFETCH_IMAGE=wayfetch:local`.

The **Container image** GitHub Actions workflow builds and runs the Compose
integration tests before publishing. Published stable releases also update
`latest`; a manual run with `ref: main` publishes `edge`. A manual version-tag
run publishes that version without updating `latest`. Every published image
also has a `sha-<full-commit>` tag, source labels, a BuildKit provenance
attestation, and an SBOM. Prefer a version or digest for deployments: `edge`
tracks development and can contain incompatible changes.

GHCR packages initially default to private. A private package requires
`docker login ghcr.io` with an account allowed to read it; public packages
can be pulled without signing in.

You need Docker with the Compose plugin, and two folders: one for Wayfetch's own data and one for downloads.

**1. Create the data folder and give both folders to the container's user** (`568:568`, TrueNAS SCALE's `apps` user, unless you set `WAYFETCH_UID`/`WAYFETCH_GID`). Change ownership of these two folders only, never recursively across a whole media dataset:

```sh
sudo mkdir -p /mnt/tank/apps/wayfetch
sudo chown 568:568 /mnt/tank/apps/wayfetch /mnt/tank/downloads
sudo chmod 700 /mnt/tank/apps/wayfetch
```

**2. Tell Compose where they are:** copy [`.env.example`](.env.example) to `deploy/.env` and set the two paths.

```sh
cp deploy/.env.example deploy/.env
nano deploy/.env        # WAYFETCH_DATA and WAYFETCH_DOWNLOADS
```

**3. Start it** (the first time, this downloads the image):

```sh
docker compose -f deploy/compose.yaml up -d
docker compose -f deploy/compose.yaml ps          # "healthy" once ready
```

On its first start, Wayfetch writes a small marker into the downloads folder. On every later start it checks for that marker and refuses to save anything if it is missing, so an unmounted ZFS/NFS dataset is never mistaken for an empty folder. If that happens, mount the dataset again and restart; if you really moved downloads to a new, empty folder, mark it once with `docker compose -f deploy/compose.yaml run --rm wayfetchd init-root --state-dir /data/state /downloads`. Compose also refuses to start if either host folder is missing (`create_host_path: false`), so nothing is written into the container by mistake.

### What lives in the data folder

| Path in `WAYFETCH_DATA` | What it is |
|---|---|
| `state/` | The engine's database and partial downloads' bookkeeping |
| `api-tokens` | API bearer tokens (`mode 600`), created on first start with one token holding every permission |
| `credential-key` | Encrypts provider accounts (such as Real-Debrid); created when the first account is added |
| `auth/` | The browser owner account, when browser sign-in is turned on |

Back up the whole folder: the account key is needed to read the accounts saved in `state/`.

### Using the API

```sh
TOKEN=$(sudo awk '!/^#/ {print $1; exit}' /mnt/tank/apps/wayfetch/api-tokens)
API=http://127.0.0.1:7878
INSTANCE=$(curl -s -H "Authorization: Bearer $TOKEN" "$API/v1/capabilities" | jq -r .instance)

curl -s -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
    -d "{\"idempotency_key\":\"$(cat /proc/sys/kernel/random/uuid)\",\"target\":\"$INSTANCE\",\"items\":[\"https://example.com/file.zip\"]}" \
    "$API/v1/inputs"
curl -s -H "Authorization: Bearer $TOKEN" "$API/v1/jobs"
```

---

## Networking & Reverse Proxy Setup

Inside the container `wayfetchd` listens on `0.0.0.0:7878`, while `compose.yaml` publishes it **only on the host's loopback** (`127.0.0.1:7878`, configurable via `WAYFETCH_PUBLISH`).

### Remote CLI/API Access over SSH

To reach the loopback port from another machine without setting up TLS, forward the port over SSH:

```sh
ssh -N -L 7878:127.0.0.1:7878 user@nas
# Then open or query http://127.0.0.1:7878 locally
```

### Browser Access Behind a TLS Reverse Proxy

Browsers require HTTPS for `__Host-wayfetch-session` cookies. Place a TLS reverse proxy in front of `127.0.0.1:7878` and enable `--auth-dir` and `--browser-origin` in `compose.yaml`:

```yaml
    command: ["run", "--state-dir", "/data/state", "--download-root", "/downloads",
              "--token-file", "/data/api-tokens", "--listen", "0.0.0.0:7878",
              "--init-root-on-first-start", "--auth-dir", "/data/auth",
              "--browser-origin", "https://wayfetch.example.lan"]
```

Read the initial one-time setup secret from `$WAYFETCH_DATA/auth/setup-secret` within 15 minutes of startup. To reset a forgotten owner password later:

```sh
docker compose -f deploy/compose.yaml stop
docker compose -f deploy/compose.yaml run --rm wayfetchd reset-owner --auth-dir /data/auth
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
