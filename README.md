# Wayfetch Downloader

A Rust download manager in development with a reusable engine, a self-hosted server UI,
and a desktop application sharing the same engine and UI.

The Cargo workspace separates the engine, protocol, networking, and application
hosts. The domain crate defines typed IDs, job states,
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
the same way through `crates/host`. `wayfetchd` also serves a bundled web
UI (`ui/`) for direct downloads. The `Dockerfile` and `deploy/compose.yaml`
run the daemon in a least-privilege container. The desktop host
(`apps/wayfetch-desktop`) embeds the engine and uses the same UI through IPC.

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
│   └── wayfetch-desktop/    # Desktop host embedding the engine
├── crates/
│   ├── domain/             # IDs, states, validated domain types
│   ├── protocol/           # Shared commands, results, errors, events
│   ├── engine/             # Reusable engine and lifecycle
│   │   └── src/
│   │       ├── store/      # Persistence and migrations
│   │       ├── fs/         # Safe roots, checkpoints, publication
│   │       └── transfer/   # Transfer contracts and streaming
│   ├── net/                # HTTP, sessions, streaming transfers
│   ├── host/               # What the CLI, daemon, and desktop hosts share
│   ├── cli/                # Foreground `wayfetch` host
│   └── api/                # HTTP authentication and REST/SSE adapter
├── ui/                     # Web UI; package.json has only dev checks
│   ├── src/components/     # Shared workflow views
│   ├── src/client/         # WayfetchClient contract, DTOs, event feed
│   ├── src/transports/     # HTTP/SSE and desktop IPC clients
│   ├── src/shell/          # Desktop window bootstrap
│   └── tests/              # Unit and real-browser tests
├── deploy/                 # Compose file for the server image
├── docs/architecture.md    # Canonical design and decision register
├── AGENTS.md               # Agent workflow and repository policy
├── LICENSE-APACHE          # Apache License 2.0
├── LICENSE-MIT             # MIT License
└── CONTRIBUTING.md         # Contribution guidance
```

Tests live alongside the crates, applications, and UI they exercise. Add shared
test directories only when a cross-component suite needs them. Provider adapters,
extraction components, and worker binaries get directories when implemented;
the suggested architecture layout does not require empty placeholders.
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

Downloads are saved in the download root until the owner picks another
default destination in Settings (`PUT /v1/settings`). The owner adds more
folders in Settings ("Add folder…", which lists the server's folders), or
the operator names them: initialize each with `init-root` and pass it to
`run` with `--root DIR`, once per folder. Any folder the daemon can open can
be added, except its state directory, so a token or session with `control`
decides where new downloads may be written. Changing the default affects
only downloads added afterwards; earlier ones keep their folder, and one
whose folder is no longer configured waits, blocked, instead of moving.

It listens on `127.0.0.1:7878` unless `--listen` says otherwise; anything
beyond loopback has no TLS of its own and belongs behind an authenticated TLS
reverse proxy. Every request needs `Authorization: Bearer TOKEN`. The token file
holds one `TOKEN PERMISSION[,PERMISSION...]` per line, with permissions `read`,
`submit`, `control`, and `sources` (reading back submitted links, which can
carry signed tokens), and must be readable only by its owner. If it does not
exist, `wayfetchd` creates it with one token holding every permission; the token
is never printed.

| Route | Purpose |
|---|---|
| `GET /v1/capabilities` | Instance ID, boot epoch, protocol version, newest event, limits |
| `POST /v1/inputs` | Submit `{"idempotency_key", "target", "items": [URL...]}` |
| `GET /v1/jobs` | A page of jobs: `after`, `page_size`, `stage=a,b`, `package` |
| `GET /v1/jobs/{id}` | One job |
| `GET /v1/job-counts` | Jobs per stage, as `{"stages": {"queued": 3, ...}, "as_of"}`: `stage=a,b`, `package` |
| `POST /v1/jobs/{id}/commands` | `{"idempotency_key", "target", "action": "pause"\|"resume"\|"cancel"\|"retry"\|"remove"\|"rename", "expected_revision"}`, plus `"name"` for a rename |
| `GET /v1/jobs/{id}/source` | The link the job was submitted with (needs `sources`) |
| `POST /v1/staging/links` | Collect links for review without downloading them: `{"idempotency_key", "target", "items": [text...], "destination"?: "root-N"}` |
| `GET /v1/staging/links` | A page of collected links: `after`, `page_size` |
| `POST /v1/staging/commands` | `{"idempotency_key", "target", "action": "start"\|"remove"\|"move", "links": [{"link", "revision"}...]}`, plus `"destination"` for a move |
| `GET /v1/staging/links/{id}/source` | A collected link as entered (needs `sources`) |
| `GET /v1/settings` | The default destination and the folders to choose from, as server paths with `available` and `problem` |
| `PUT /v1/settings` | Choose the default: `{"idempotency_key", "target", "expected_revision", "default_root": "root-N"}` (needs `control`) |
| `GET /v1/folders` | The folders inside `path` (absolute; the top of the filesystem if absent), without hidden ones, at most 1,000 (needs `control`) |
| `POST /v1/roots` | Add the server folder `{"path"}` as a destination, returning it with its `id` (needs `control`) |
| `POST /v1/roots/remove` | Stop offering a folder the user added (`{"idempotency_key", "target", "expected_revision", "root"}`); its files stay (needs `control`) |
| `GET /v1/events` | Server-sent events: `hello`, `event`, `progress`, `resync` |
| `GET /v1/ready` | Readiness, without a token: `200 {"ready":true}`, or `503` with a `reason` |
| `GET /v1/auth/session`, `POST /v1/auth/{setup,login,logout}` | Browser sign-in; see [Browser access](#browser-access) |

`rename` gives a completed job's file a new name in the same folder; it never
replaces a file, and refuses one changed outside Wayfetch. `remove` takes a
complete, failed, or cancelled job off lists and counts and keeps its files.
`retry` starts a failed, or fully stopped cancelled, job again as the same
job, with the same link, folder, and collected-links entry. It continues
the data already saved when the server still offers the same file and
accepts the request for the rest, and otherwise starts from zero, with the
reason in the job's `restart_reason`. A failure that names a time to wait
until is refused with `not_applicable` and that `retry_at`. Like the other
job actions it needs `control`.

`/v1/inputs` starts downloads at once. Collected links instead stay listed
until removed, survive restarts, and are never fetched until started; a
started link stays listed with its `job`, and an entry that is not a link is
kept with its problem. A staging command reports each link it applied and
each it refused, with a reason, so a link changed since its `revision` was
read, or bound for an unavailable folder, stays collected. Each link starts
at most one download. Collecting and starting need `submit`; removing or
moving links, which may be someone else's, needs `control`.

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

### Browser access

Browser sign-in is off unless you give `wayfetchd run` an auth directory and
the origin browsers reach it under. Browsers need HTTPS, so the daemon sits
behind a TLS reverse proxy and `--browser-origin` is the proxy's address:

```sh
wayfetchd run --state-dir /srv/wayfetch/state --download-root /srv/downloads \
    --token-file /srv/wayfetch/api-tokens \
    --auth-dir /srv/wayfetch/auth --browser-origin https://wayfetch.example.lan
```

`--auth-dir` holds the owner account, a setup secret, and a lock. It is
created owner-only (`700`) if missing, and the daemon refuses it if other
users can read it. `--browser-origin` is repeatable, takes a scheme, host,
and optional port only, and must be `https://`.

**Setup.** On a start with no owner account, the daemon writes a single-use
setup secret to `AUTH_DIR/setup-secret` (mode `600`) and logs only the file's
path. Within 15 minutes, open the origin in a browser and enter the secret
with the owner name and a password of at least 12 characters (see
[The web UI](#the-web-ui)). Without a browser, send the same to
`POST /v1/auth/setup` with `curl` and `jq`; the password is read without echo
and passed through the environment and standard input, never as an argument:

```sh
ORIGIN=https://wayfetch.example.lan          # the --browser-origin
SECRET=$(sudo cat /srv/wayfetch/auth/setup-secret)
read -rsp 'Owner password: ' PASSWORD; echo
(umask 077 && : > cookies.txt) && chmod 600 cookies.txt   # the session: owner-only
SECRET=$SECRET PASSWORD=$PASSWORD \
    jq -n '{setup_secret: env.SECRET, username: "owner", password: env.PASSWORD}' |
    curl -sS --fail-with-body -c cookies.txt -H "Origin: $ORIGIN" \
        -H 'Content-Type: application/json' --data-binary @- "$ORIGIN/v1/auth/setup"
unset SECRET PASSWORD
```

Before the reverse proxy is in place, run the same `curl` on the server
against the daemon itself, naming the origin's host:
`... -H 'Host: wayfetch.example.lan' --data-binary @- http://127.0.0.1:7878/v1/auth/setup`.

A successful setup creates the local owner account, answers
`{"authenticated":true,...,"csrf_token":...}`, and stores the session cookie
in `cookies.txt`, which is created owner-only first because the cookie is
the session; the secret is then deleted. Delete `cookies.txt` when done. A wrong secret creates
nothing; an expired one is refused and removed, and a restart writes a new
one. Once an owner exists, further setups are refused (`409`), even
concurrent ones or from a second process. The account file stores an
Argon2id hash of the password, never the password.

**Signing in.** `POST /v1/auth/login` with `{"username", "password"}` sets
an opaque session cookie (`__Host-wayfetch-session`: `Secure`, `HttpOnly`,
`SameSite=Strict`, host-only) and returns a `csrf_token`. Every failure
answers only `authentication failed`. Sign-in and setup attempts share a
limit of five a minute for the whole server, so excess attempts get `429`
with `Retry-After` even with the right password; existing sessions and API
tokens keep working meanwhile. `GET /v1/auth/session` tells a page whether
setup is pending and, when signed in, the owner's name and the CSRF token.
`POST /v1/auth/logout` ends the session. With `curl`:

```sh
read -rsp 'Owner password: ' PASSWORD; echo
(umask 077 && : > cookies.txt) && chmod 600 cookies.txt
CSRF=$(PASSWORD=$PASSWORD jq -n '{username: "owner", password: env.PASSWORD}' |
    curl -sS --fail-with-body -c cookies.txt -H "Origin: $ORIGIN" \
        -H 'Content-Type: application/json' --data-binary @- "$ORIGIN/v1/auth/login" |
    jq -r .csrf_token)
unset PASSWORD
curl -sS -b cookies.txt -H "Origin: $ORIGIN" "$ORIGIN/v1/jobs"
curl -sS -b cookies.txt -H "Origin: $ORIGIN" -H "X-CSRF-Token: $CSRF" -X POST \
    "$ORIGIN/v1/auth/logout"
```

A signed-in browser can use every route above, including `/v1/events`, with
all permissions. Signing out, expiry, or replacement of a session also ends
the event streams opened with it. State-changing requests must send the session's CSRF token
in `X-CSRF-Token`. Every cookie-authenticated request must carry a `Host`
of a configured origin (which stops DNS rebinding), and an `Origin` header
equal to one where one is sent, which every `POST` must; requests a browser
marks `Sec-Fetch-Site: cross-site` or `same-site` are refused, and no CORS
headers are ever sent. A request with an `Authorization` header is judged
by its bearer token alone, exactly as before.

**Sessions** live in memory only: a session ends after 12 hours unused, 7
days after sign-in, on sign-out, or when the daemon restarts, which signs
every browser out (the owner account stays). At most 32 sessions exist; a
new one beyond that ends the least recently used.

**Recovery.** If the owner password is lost, stop the daemon, remove the
owner, and start again to get a new setup secret. Downloads, jobs, and API
tokens are untouched. `reset-owner` is refused while a daemon uses the
directory.

```sh
wayfetchd reset-owner --auth-dir /srv/wayfetch/auth
```

**Reverse proxy requirements.** The proxy terminates TLS for the origin and
forwards to the daemon's loopback address. It must pass the original `Host`
(or set it to the origin's host) and leave `Origin`, `Cookie`,
`X-CSRF-Token`, and `Sec-Fetch-*` untouched; it must not add CORS headers;
and it must not buffer `/v1/events` or time it out quickly. Caddy does all of
this by default:

```text
wayfetch.example.lan {
    reverse_proxy 127.0.0.1:7878
}
```

With nginx, in the TLS `server` block:

```nginx
location / {
    proxy_pass http://127.0.0.1:7878;
    proxy_set_header Host $host;
    proxy_http_version 1.1;
    proxy_buffering off;          # server-sent events
    proxy_read_timeout 1h;
}
```

**Loopback development.** `--dev-insecure-loopback` allows sign-in over plain
HTTP, with a cookie that lacks `Secure` (`wayfetch-session`), for
development on the same machine. It is refused unless `--listen` is a
loopback address and every origin is plain-HTTP loopback; without
`--browser-origin`, the origins are the listener's address and `localhost`
on its port. Never use it for a deployment.

### The web UI

`wayfetchd` serves its web UI at `/` on the same origin as the API; it is
compiled into the binary, and the page loads nothing from anywhere else, so
it works with no internet access. With browser access configured, open the
origin, complete setup the first time, and sign in. To try it on one
machine:

```sh
wayfetchd run --state-dir /tmp/wf/state --download-root /tmp/wf/downloads \
    --token-file /tmp/wf/api-tokens --auth-dir /tmp/wf/auth --dev-insecure-loopback
# open http://127.0.0.1:7878 and enter the secret from /tmp/wf/auth/setup-secret
```

The layout is a dense download table for a desktop browser: a toolbar
(add links, pause, resume, cancel, or retry the selected job, filter by name,
Settings), state filters with job counts on the left, the table (each job's
name, the host of its link, state, progress, size, speed, and time left),
details of the selected job below the table (its file name and kind, the
folder it saves to, and the link it came from, shown without its query,
which can carry a signed token, with a button to copy the whole link), and
a status bar with the connection state. In the desktop app the folder opens
in the file manager once the file is saved; a browser shows it as a server
path. The add form says which folder new downloads go to. It
follows the system's light or dark theme and uses the bundled IBM Plex Sans
typeface (`ui/src/fonts/`, SIL Open Font License 1.1).

Settings chooses the default folder for new downloads among those the
server permits, shown as server paths (jobs already added keep theirs),
and the row density, which is kept in this browser. Its Accounts &
services section is a table of the accounts added (none yet). Add
account opens a searchable catalogue of file hosts, cloud storage and
multi-host services to pick its service from, led by Real-Debrid, found
by name, by any of their domains, or by a pasted link. Every entry says
Not available yet and none can be added; direct links never need an
account. Each row has a menu, from its "…"
button, a right-click, or Shift+F10 or the Menu key: rename a finished
file in its folder (never replacing another file), copy the link it came
from (with the `sources` permission), pause, resume, cancel, or retry a
failed or cancelled download (continuing its saved data when it can; its
details say why when it had to start over), and remove
a finished, failed, or cancelled job from the list, which keeps its file. A browser cannot open
files on the server, so it does not offer to.

The UI adds direct links (one per line), lists jobs a page of 50 at a time in
the order they were added, shows one job at `/jobs/job-N`, and pauses,
resumes, or cancels them. It follows the event stream and reconnects on its
own, resuming from the last event it saw, or re-reading the list when the
server says to resynchronize. Durable progress moves at each 8 MiB
checkpoint without an event, so while a shown job is active the page
re-reads its 50 jobs every few seconds. Reloading or closing the tab never
affects a download. When the session ends (sign-out in any tab, expiry, or a
daemon restart, which signs every browser out) the page says so and asks to
sign in again; downloads keep running meanwhile.

Names and messages from downloads are always shown as text. The page's
Content Security Policy allows scripts, styles, fonts, and requests only from its
own origin, forbids framing, and enables Trusted Types, so the browser
refuses to parse any string as HTML. The page is revalidated on every load,
and its scripts, styles, and fonts are served below `/assets/VERSION/`, where
`VERSION` is a digest of the bundle, and cached for good.

## Running the desktop app

`wayfetch-desktop` is a proof of the desktop host (architecture brief
sections 11.6 and 11.7): it runs the engine inside its own process and
shows the same Downloads views as the web UI in a window, using the
system WebView through [Tauri](https://tauri.app) 2. There is no daemon
and no listening socket: the page reaches the engine only through a fixed
set of IPC commands (`apps/wayfetch-desktop/src/ipc.rs`) that carry the
same JSON as the HTTP API, and its capability
(`apps/wayfetch-desktop/capabilities/main.json`) grants those commands and
no Tauri plugin, so the page has no filesystem, shell, or network access.
The UI files are compiled into the binary from `ui/src` as they are; the
window's page is `ui/src/shell/desktop.html`, which needs no sign-in.

Only Linux is built and tested so far. Building needs WebKitGTK 4.1 and
GTK 3 development packages (on Debian or Ubuntu: `libwebkit2gtk-4.1-dev
libgtk-3-dev libsoup-3.0-dev librsvg2-dev`); running needs WebKitGTK 4.1.
Installers, a tray, and default state paths are not part of the proof
yet, so the directories are named as for the daemon:

```sh
cargo build --locked -p wayfetch-desktop
target/debug/wayfetch-desktop init-root --state-dir /tmp/wf/state /tmp/wf/downloads
target/debug/wayfetch-desktop run --state-dir /tmp/wf/state --download-root /tmp/wf/downloads
```

The engine starts before the window opens, so a state directory another
engine owns (a daemon, the CLI, or another window) is refused with no
window shown. Closing the window, `SIGINT`, or `SIGTERM` stops the engine
gracefully and exits; unfinished downloads continue on the next start.
Reloading the page, or losing its event stream, never affects a download:
the page reads a new snapshot and follows events from there. The host
sends each page at most 64 event items it has not acknowledged and holds
at most 4 subscriptions per page; a page that stops reading for 15 seconds
loses its subscription, and a reload releases them all.

Settings' "Choose folder…" opens the system's folder dialog from the host;
the page never sends a path. The engine marks the chosen folder as a
destination, as `init-root` does, and it stays one across restarts. A
cancelled dialog, or a folder that cannot be used, changes nothing. A
finished job's menu also opens its file, or the folder holding it, with
this computer's default application (`xdg-open`): the page names the job,
and the host opens only the file the engine published for it.

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
`WAYFETCH_GID`, `568:568` by default (the TrueNAS SCALE apps identity).
Compose refuses to start if a path is missing, and never creates one.
Change ownership of these directories only, never recursively on a broader
dataset:

```sh
export WAYFETCH_STATE=/srv/wayfetch/state WAYFETCH_TOKENS=/srv/wayfetch/tokens \
       WAYFETCH_DOWNLOADS=/srv/downloads
mkdir -p "$WAYFETCH_STATE" "$WAYFETCH_TOKENS"
sudo chown 568:568 "$WAYFETCH_STATE" "$WAYFETCH_TOKENS" "$WAYFETCH_DOWNLOADS"
sudo chmod 700 "$WAYFETCH_STATE" "$WAYFETCH_TOKENS"
```

Put these variables in a `.env` file next to `compose.yaml` to keep them.
Pass the same `-f deploy/compose.yaml` and variables to every command below.

**Initialize the root and start.** Initializing the root is a separate,
one-time step, so an unmounted dataset's empty mount point is never mistaken
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
ssh -N -L 7878:127.0.0.1:7878 user@nas      # then use http://127.0.0.1:7878
```

The API has no TLS of its own, so do not publish it on a LAN or public
address directly; put an authenticated TLS reverse proxy in front of it for
that.

**Browser access** in the container works through such a proxy on the host,
forwarding to the published loopback port (see the reverse proxy
requirements above). Add the auth directory inside the token mount and the
proxy's origin to the image's command, for example by overriding `command`
in `compose.yaml`:

```yaml
    command: ["run", "--state-dir", "/state", "--download-root", "/downloads",
              "--token-file", "/tokens/api-tokens", "--listen", "0.0.0.0:7878",
              "--auth-dir", "/tokens/auth",
              "--browser-origin", "https://wayfetch.example.lan"]
```

The setup secret is then `$WAYFETCH_TOKENS/auth/setup-secret` on the host,
and owner reset runs with the container stopped:

```sh
docker compose -f deploy/compose.yaml stop
docker compose -f deploy/compose.yaml run --rm wayfetchd reset-owner --auth-dir /tokens/auth
docker compose -f deploy/compose.yaml start
```

## Working on the project

Read [CONTRIBUTING.md](CONTRIBUTING.md), [AGENTS.md](AGENTS.md), and the relevant
sections of [the architecture brief](docs/architecture.md) before implementation.
The brief describes intended behavior and acceptance gates, not existing features.

### Developer commands

You need `rustup`, Python 3, and a C compiler (the engine builds its bundled
SQLite); the desktop host also needs the WebKitGTK packages above. The Rust toolchain is pinned in
`rust-toolchain.toml`; `rustup toolchain install` (or the first `cargo` command)
installs it with rustfmt and Clippy. No JavaScript or desktop tooling is needed
for the engine or for `wayfetchd`: the web UI is plain JavaScript modules,
served as they are, with no build step. These are the checks CI runs on
every push and pull request:

```sh
cargo fmt --all -- --check                                   # formatting
cargo clippy --locked --workspace --exclude wayfetch-desktop --all-targets --all-features -- -D warnings
cargo test --locked --workspace --exclude wayfetch-desktop --all-features  # also writes UI types
cargo build --locked -p wayfetch-engine                      # engine alone
python3 scripts/check_dependency_boundaries.py               # crate boundaries
python3 -m unittest discover -s scripts                       # tests for the check
```

The web UI's checks need Node.js 22 or later. `npm ci` installs only the
development tools: TypeScript, which type-checks the JavaScript through its
JSDoc annotations, and Playwright. The browser tests drive Chromium against a
debug `wayfetchd` (`WAYFETCHD` names another binary) and a local fixture
server, with every address beyond loopback unreachable:

```sh
cd ui && npm ci
npm run check                                     # types
npm test                                          # unit tests (node --test)
cargo build --locked -p wayfetchd && npm run e2e  # real browser
```

`npx playwright install chromium` fetches the browser if Playwright has none.

The UI's types for the API's JSON are generated from the Rust definitions by
[ts-rs](https://github.com/Aleph-Alpha/ts-rs), behind each wire crate's `ts`
feature. Tests with that feature (as in `--all-features` above) write them to
`ui/src/client/generated`, where `npm run check` reads them; commit them
with the Rust change. CI regenerates them from scratch and fails if they
differ from the committed files.

The desktop host is a workspace member but not a default member, so plain
`cargo build` and the commands above leave it out; check it on its own.
Its window tests drive the real app through `WebKitWebDriver` (Debian and
Ubuntu package `webkit2gtk-driver`) on an X display, so they are ignored by
default:

```sh
cargo clippy --locked -p wayfetch-desktop --all-targets -- -D warnings
cargo test --locked -p wayfetch-desktop                       # unit tests
xvfb-run -a cargo test --locked -p wayfetch-desktop --test shell -- --ignored
```

The container tests need Docker with the Compose plugin, so they are ignored
by default. They build `wayfetch:local` with `docker build` first, or test
the image named by `WAYFETCH_IMAGE`:

```sh
cargo test --locked -p wayfetchd --test container -- --ignored
```

`scripts/check_dependency_boundaries.py` fails if the domain, protocol, engine,
net, host, or CLI crates gain a server, desktop, or UI-toolkit dependency (for
example Axum, Tauri, or GTK) or embed the web UI (`rust-embed`), if the engine-side crates depend on the host
crate, if the HTTP adapter or daemon gain a desktop dependency, if the
desktop host gains the HTTP adapter, a server framework, or the daemon, if the domain
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
drive the built `wayfetchd` over its HTTP API. The desktop window tests
(`apps/wayfetch-desktop/tests/shell.rs`) add, pause, resume, and cancel
downloads through the shared UI in the real window, reload it mid-transfer,
close it as a window manager does, and relaunch on the same state
directory to finish the download; they also check a second engine is
refused, protocol errors and resynchronization over IPC, the subscription
limits, that the page loads only bundled files and can call no Tauri
plugin, and that the app starts no daemon and listens on no TCP port. The browser test
(`ui/tests/e2e/browser.spec.js`) sets up the owner, downloads, pauses,
resumes, and cancels through the UI, reloads and closes the tab, restarts the
daemon, and signs out, from a fresh browser profile. The container tests
(`apps/wayfetchd/tests/container.rs`) run the image through
`deploy/compose.yaml` with a fixture server on the host's Docker bridge
address: the image serves the UI, a download resumes after the container is
removed and recreated, `docker compose stop` is graceful, a second container cannot take the state
directory, and a missing or uninitialized root is refused.

Use `cargo fmt --all` to apply formatting.

## License

Wayfetch Downloader is dual-licensed under [MIT](LICENSE-MIT) or
[Apache-2.0](LICENSE-APACHE), at your option.
