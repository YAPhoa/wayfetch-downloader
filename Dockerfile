# syntax=docker/dockerfile:1
#
# The wayfetchd server image (architecture brief section 11.4): built with
# the toolchain pinned in rust-toolchain.toml and the locked dependencies,
# and run as a non-root user with no shell or package manager. See
# deploy/README.md and deploy/compose.yaml.

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
# hash in deploy/plugins, which the desktop packages share: yt-dlp with
# its YouTube player-code scripts as one zip program run by the image's
# Python, and deno, which runs those scripts.
FROM ${PLUGINS_IMAGE} AS plugins
COPY deploy/plugins /plugins
RUN /plugins/build.sh /out /usr/bin/python3

FROM ${FFMPEG_IMAGE} AS ffmpeg

FROM ${RUNTIME_IMAGE}
COPY --from=build /out/wayfetchd /usr/local/bin/wayfetchd
# Found on PATH, so no configuration names them.
COPY --from=plugins /out/yt-dlp /out/deno /usr/local/bin/
COPY --from=ffmpeg /ffmpeg /usr/local/bin/ffmpeg
# Their licenses; `ffmpeg -L` prints FFmpeg's, and versions.json names the
# libraries its static build includes.
COPY --from=plugins /out/licenses /usr/local/share/licenses/
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
