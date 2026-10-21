#!/bin/sh
# Builds the Linux desktop packages, a .deb and an .rpm, with the yt-dlp
# and deno plugins (deploy/plugins) under /usr/lib/wayfetch-downloader.
# Needs the WebKitGTK development packages (see the README), Python 3 with
# pip, and the Tauri CLI (`cargo install tauri-cli --version 2.12.3
# --locked`). Build on the oldest distribution the packages should run
# on: they need its glibc or newer.
#
# Writes target/release/bundle/{deb,rpm}/, named after the package
# (wayfetch-downloader_VERSION_ARCH.deb, wayfetch-downloader-VERSION-1.ARCH.rpm),
# and their SHA256SUMS.
#
# With TAURI_SIGNING_PRIVATE_KEY (and TAURI_SIGNING_PRIVATE_KEY_PASSWORD)
# set, also signs each package for the app's updater (a .sig beside it)
# and writes latest.json, which installed apps read from the newest
# published release.
set -eu
cd "$(dirname "$0")/.."
rm -rf target/desktop-plugins target/release/bundle
deploy/plugins/build.sh target/desktop-plugins /usr/bin/python3
signing=
if [ -n "${TAURI_SIGNING_PRIVATE_KEY:-}" ]; then
    signing='{"bundle":{"createUpdaterArtifacts":true}}'
fi
# Stripped here rather than in the workspace profile, which the server
# image's build also uses.
(cd apps/wayfetch-desktop && CARGO_PROFILE_RELEASE_STRIP=true cargo tauri build --ci \
    ${signing:+--config "$signing"} -- --locked)
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
repository=$(sed -n 's/^repository = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
cd target/release/bundle
# Tauri names the files after the product name, which has a space.
for file in deb/*.deb deb/*.deb.sig rpm/*.rpm rpm/*.rpm.sig; do
    [ -e "$file" ] || continue
    mv "$file" "$(dirname "$file")/wayfetch-downloader${file#*/Wayfetch Downloader}"
done
sha256sum deb/*.deb rpm/*.rpm > SHA256SUMS
cat SHA256SUMS
if [ -n "$signing" ]; then
    python3 ../../../scripts/updater_manifest.py "$version" \
        "$repository/releases/download/v$version" deb/*.deb rpm/*.rpm > latest.json
    cat latest.json
fi
