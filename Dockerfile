# syntax=docker/dockerfile:1
#
# The wayfetchd server image (architecture brief section 11.4): built with
# the toolchain pinned in rust-toolchain.toml and the locked dependencies,
# and run as a non-root user with no shell or package manager. See the
# README's "Running in a container" and deploy/compose.yaml.

# Base images are pinned by digest; update tag and digest together. The
# Rust tag names the toolchain in rust-toolchain.toml (a packaging test
# checks that they agree). Both can be overridden, for example with a
# mirror that serves the same images.
ARG BUILDER_IMAGE=docker.io/library/rust:1.97.0-slim-bookworm@sha256:6d220bf85c74e842a79da63997af8d2e74455c0b8847d8bb3a5888572334991d
# Installs the yt-dlp plugin's Python packages; any Python 3 with pip.
ARG PLUGINS_IMAGE=docker.io/library/python:3.11-slim-bookworm@sha256:0a310eeecf4e1f5a0743f9a6520c90c88d089c903ca5fd283f501e3a805f5f89
# A static ffmpeg built with libmp3lame (GPL; see the README's licenses).
ARG FFMPEG_IMAGE=docker.io/mwader/static-ffmpeg:7.1.1@sha256:11a44711684c0b9f754c047dcd64235b8b52deab251bd0e0a86f22faa160749c
# glibc, libgcc, the Debian CA bundle, and Python 3.11 for yt-dlp; same
# Debian release as the builder, so the binary's glibc matches.
ARG RUNTIME_IMAGE=gcr.io/distroless/python3-debian12:nonroot@sha256:7d1042ce588ab97019fe95c24ffca7bc5a82ccdac572511d5e09bda4435c89c5

FROM ${BUILDER_IMAGE} AS build
WORKDIR /src
COPY rust-toolchain.toml Cargo.toml Cargo.lock ./
COPY crates crates
COPY apps apps
# Embedded in the binary: the image serves its UI with no other runtime.
COPY ui/src ui/src
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --locked --release -p wayfetchd \
    && install -D target/release/wayfetchd /out/wayfetchd

# The plugins Settings > Plugins shows (ADR-27), pinned by version and
# hash: yt-dlp with its YouTube player-code scripts as one zip program
# run by the image's Python, and deno, which runs those scripts. Update a
# version and its hashes together (https://pypi.org/pypi/NAME/VERSION/json).
FROM ${PLUGINS_IMAGE} AS plugins
COPY <<REQUIREMENTS /tmp/yt-dlp.txt
yt-dlp==2026.8.19 --hash=sha256:1d57897e94c6665a0a6f9bc54b34e584284e32c034ffab3a7df25d8f7b24eedf
yt-dlp-ejs==0.8.0 --hash=sha256:79300e5fca7f937a1eeede11f0456862c1b41107ce1d726871e0207424f4bdb4
REQUIREMENTS
# The x86_64 and aarch64 builds.
COPY <<REQUIREMENTS /tmp/deno.txt
deno==2.9.5 --hash=sha256:e5aec0419739fd0359663a78765d4feda19d1d0b60a682d25add9b78e19f95b4 --hash=sha256:bccec0965917b6daa323f9ce6ad8e884e0854506b0cdaec9de41bdac7620109a
REQUIREMENTS
RUN set -eu; \
    install='python3 -m pip install --no-cache-dir --disable-pip-version-check --no-deps --only-binary=:all: --require-hashes'; \
    $install --target /tmp/yt-dlp -r /tmp/yt-dlp.txt; \
    $install --target /tmp/deno -r /tmp/deno.txt; \
    rm -rf /tmp/yt-dlp/bin; \
    mkdir /out; \
    python3 -m zipapp /tmp/yt-dlp --compress --python /usr/bin/python3 --main yt_dlp:main --output /out/yt-dlp; \
    chmod 0755 /out/yt-dlp; \
    install -m 0755 /tmp/deno/bin/deno /out/deno; \
    install -D -m 0644 /tmp/yt-dlp/yt_dlp-*.dist-info/licenses/LICENSE /out/licenses/yt-dlp/LICENSE; \
    install -D -m 0644 /tmp/yt-dlp/yt_dlp_ejs-*.dist-info/licenses/LICENSE /out/licenses/yt-dlp-ejs/LICENSE; \
    install -D -m 0644 /tmp/deno/deno-*.dist-info/licenses/LICENSE /out/licenses/deno/LICENSE

FROM ${FFMPEG_IMAGE} AS ffmpeg

# 7-Zip, which unpacks downloaded archives: the official static build,
# pinned by version and hash for each architecture. Update the version and
# both hashes together (https://github.com/ip7z/7zip/releases).
FROM ${PLUGINS_IMAGE} AS sevenzip
ARG TARGETARCH
COPY <<'FETCH' /tmp/fetch.py
import hashlib, io, os, sys, tarfile, urllib.request
version = "26.04"
builds = {
    "amd64": ("x64", "fc0327ba27e89bd086cf426dff17d77de582953cdbbc10a6576540a06853ffcd"),
    "arm64": ("arm64", "5b0ac3aa91c1e3499f011d6af69d7f6b4de3ef2f011d9e2ff44c58ebd04aa388"),
}
arch, digest = builds[sys.argv[1]]
name = f"7z{version.replace('.', '')}-linux-{arch}.tar.xz"
url = f"https://github.com/ip7z/7zip/releases/download/{version}/{name}"
data = urllib.request.urlopen(url, timeout=120).read()
if hashlib.sha256(data).hexdigest() != digest:
    sys.exit(f"{name} does not match its pinned hash")
with tarfile.open(fileobj=io.BytesIO(data), mode="r:xz") as archive:
    for member, target, mode in (("7zzs", "/out/7zz", 0o755), ("License.txt", "/out/licenses/7-zip/License.txt", 0o644)):
        os.makedirs(os.path.dirname(target), exist_ok=True)
        with open(target, "wb") as out:
            out.write(archive.extractfile(member).read())
        os.chmod(target, mode)
FETCH
RUN python3 /tmp/fetch.py "${TARGETARCH}"

FROM ${RUNTIME_IMAGE}
COPY --from=build /out/wayfetchd /usr/local/bin/wayfetchd
# Found on PATH, so no configuration names them.
COPY --from=plugins /out/yt-dlp /out/deno /usr/local/bin/
COPY --from=ffmpeg /ffmpeg /usr/local/bin/ffmpeg
COPY --from=sevenzip /out/7zz /usr/local/bin/7zz
# Their licenses; `ffmpeg -L` prints FFmpeg's, and versions.json names the
# libraries its static build includes.
COPY --from=plugins /out/licenses /usr/local/share/licenses/
COPY --from=sevenzip /out/licenses /usr/local/share/licenses/
COPY --from=ffmpeg /versions.json /usr/local/share/licenses/ffmpeg/versions.json
# Distroless's unprivileged user; Compose sets the deployment's UID/GID.
USER 65532:65532
EXPOSE 7878
# wayfetchd drains the API and stops the engine gracefully on SIGTERM.
STOPSIGNAL SIGTERM
# Asks the running daemon over loopback; starts no engine, needs no token.
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --start-interval=1s --retries=3 \
    CMD ["/usr/local/bin/wayfetchd", "ready"]
ENTRYPOINT ["/usr/local/bin/wayfetchd"]
# Every path is a mount; none exists in the image, so a missing mount
# fails instead of writing into the container.
CMD ["run", "--state-dir", "/state", "--download-root", "/downloads", \
     "--token-file", "/tokens/api-tokens", "--listen", "0.0.0.0:7878"]
