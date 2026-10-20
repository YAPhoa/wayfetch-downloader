#!/bin/sh
# Builds the yt-dlp and deno plugins from the versions and hashes pinned
# beside this script, for the server image and the desktop packages alike:
#
#   build.sh OUT INTERPRETER
#
# OUT receives `yt-dlp` (a zip program run by INTERPRETER, the Python 3
# that will run it where it is installed), `deno`, and their licenses.
# Needs Python 3 with pip; downloads only the pinned wheels.
set -eu
out=$1
interpreter=$2
pins=$(dirname "$0")
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
install='python3 -m pip install --no-cache-dir --disable-pip-version-check --no-deps --only-binary=:all: --require-hashes'
$install --target "$work/yt-dlp" -r "$pins/yt-dlp.txt"
$install --target "$work/deno" -r "$pins/deno.txt"
rm -rf "$work/yt-dlp/bin"
mkdir -p "$out"
python3 -m zipapp "$work/yt-dlp" --compress --python "$interpreter" --main yt_dlp:main --output "$out/yt-dlp"
chmod 0755 "$out/yt-dlp"
install -m 0755 "$work/deno/bin/deno" "$out/deno"
install -D -m 0644 "$work"/yt-dlp/yt_dlp-*.dist-info/licenses/LICENSE "$out/licenses/yt-dlp/LICENSE"
install -D -m 0644 "$work"/yt-dlp/yt_dlp_ejs-*.dist-info/licenses/LICENSE "$out/licenses/yt-dlp-ejs/LICENSE"
install -D -m 0644 "$work"/deno/deno-*.dist-info/licenses/LICENSE "$out/licenses/deno/LICENSE"
