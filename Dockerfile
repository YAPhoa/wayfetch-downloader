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
# glibc, libgcc, and the Debian CA bundle; same Debian release as the
# builder, so the binary's glibc matches.
ARG RUNTIME_IMAGE=gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f

FROM ${BUILDER_IMAGE} AS build
WORKDIR /src
COPY rust-toolchain.toml Cargo.toml Cargo.lock ./
COPY crates crates
COPY apps apps
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --locked --release -p wayfetchd \
    && install -D target/release/wayfetchd /out/wayfetchd

FROM ${RUNTIME_IMAGE}
COPY --from=build /out/wayfetchd /usr/local/bin/wayfetchd
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
