#!/usr/bin/env python3
"""Write the desktop app's update manifest, latest.json, for a release.

The desktop app reads it from the newest published release
(releases/latest/download/latest.json) and installs the package for how it
was installed, .deb or .rpm, after checking the package's signature. Each
package needs its .sig file beside it, which `cargo tauri build` writes
when TAURI_SIGNING_PRIVATE_KEY is set.

Usage: updater_manifest.py VERSION BASE_URL PACKAGE... > latest.json
where BASE_URL is the release's download address, such as
https://github.com/OWNER/REPO/releases/download/v0.2.0
"""

from datetime import datetime, timezone
import json
from pathlib import Path
import re
import sys

# How the updater names each package kind and the machine it is for.
KINDS = {".deb": "deb", ".rpm": "rpm"}
ARCHES = {"amd64": "x86_64", "x86_64": "x86_64", "arm64": "aarch64", "aarch64": "aarch64"}


def platform(package: Path) -> str:
    """The manifest key for `package`, such as linux-x86_64-deb."""
    kind = KINDS.get(package.suffix)
    if kind is None:
        raise ValueError(f"{package.name}: not a .deb or .rpm")
    found = re.search(r"[._-](amd64|x86_64|arm64|aarch64)$", package.stem)
    if found is None:
        raise ValueError(f"{package.name}: no machine architecture in the name")
    return f"linux-{ARCHES[found.group(1)]}-{kind}"


def manifest(version: str, base_url: str, packages: list[Path], now: datetime) -> dict:
    platforms = {}
    for package in packages:
        signature = package.with_name(package.name + ".sig")
        if not signature.is_file():
            raise ValueError(f"{package.name}: no signature beside it ({signature.name})")
        key = platform(package)
        if key in platforms:
            raise ValueError(f"{package.name}: a second package for {key}")
        platforms[key] = {
            "signature": signature.read_text().strip(),
            "url": f"{base_url.rstrip('/')}/{package.name}",
        }
    if not platforms:
        raise ValueError("no packages given")
    return {
        "version": version.removeprefix("v"),
        "pub_date": now.strftime("%Y-%m-%dT%H:%M:%SZ"),
        "platforms": platforms,
    }


def main(args: list[str]) -> int:
    if len(args) < 3:
        print(__doc__.strip(), file=sys.stderr)
        return 2
    version, base_url, *packages = args
    try:
        result = manifest(version, base_url, [Path(p) for p in packages], datetime.now(timezone.utc))
    except ValueError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    json.dump(result, sys.stdout, indent=2)
    print()
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
