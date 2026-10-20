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
set -eu
cd "$(dirname "$0")/.."
rm -rf target/desktop-plugins target/release/bundle
deploy/plugins/build.sh target/desktop-plugins /usr/bin/python3
# Stripped here rather than in the workspace profile, which the server
# image's build also uses.
(cd apps/wayfetch-desktop && CARGO_PROFILE_RELEASE_STRIP=true cargo tauri build --ci -- --locked)
cd target/release/bundle
# Tauri names the files after the product name, which has a space.
for file in deb/*.deb rpm/*.rpm; do
    mv "$file" "$(dirname "$file")/wayfetch-downloader${file#*/Wayfetch Downloader}"
done
sha256sum deb/*.deb rpm/*.rpm > SHA256SUMS
cat SHA256SUMS
