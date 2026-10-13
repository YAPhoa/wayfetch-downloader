#!/usr/bin/env python3
"""Check the crate dependency boundaries in docs/architecture.md section 4.5.

Fails when a guarded workspace package has a forbidden crate anywhere in its
normal or build dependency graph for any target. Guarded packages that do not
exist yet are skipped, so rules for future hosts can be listed up front.
"""

from fnmatch import fnmatch
import json
import subprocess
import sys

# Server, desktop, and UI toolkits the engine-side crates must never pull in.
HOST_AND_UI = [
    "axum", "axum-*",
    "tauri", "tauri-*", "wry", "tao",
    "webkit2gtk*", "javascriptcore*", "gtk", "gtk-*", "gdk", "gdk-*",
    "wayfetchd", "wayfetch-api", "wayfetch-desktop", "wayfetch-cli",
]
DESKTOP = [
    "tauri", "tauri-*", "wry", "tao",
    "webkit2gtk*", "javascriptcore*", "gtk", "gtk-*", "gdk", "gdk-*",
    "wayfetch-desktop",
]

RULES = {
    "wayfetch-domain": HOST_AND_UI + ["wayfetch-protocol", "wayfetch-engine"],
    "wayfetch-protocol": HOST_AND_UI + ["wayfetch-engine"],
    "wayfetch-engine": HOST_AND_UI,
    "wayfetchd": DESKTOP,
}


def cargo(*args: str) -> str:
    result = subprocess.run(["cargo", *args], capture_output=True, text=True)
    if result.returncode != 0:
        sys.stderr.write(result.stderr)
        sys.exit(f"cargo {args[0]} failed with exit status {result.returncode}")
    return result.stdout


def main() -> int:
    metadata = json.loads(
        cargo("metadata", "--locked", "--no-deps", "--format-version", "1")
    )
    members = {p["name"] for p in metadata["packages"]}
    errors = []
    checked = 0
    for package, forbidden in RULES.items():
        if package not in members:
            continue
        checked += 1
        tree = cargo(
            "tree", "--locked", "-p", package, "-e", "no-dev",
            "--target", "all", "--prefix", "none", "-f", "{p}",
        )
        deps = {line.split()[0] for line in tree.splitlines() if line.strip()}
        deps.discard(package)
        for dep in sorted(deps):
            if any(fnmatch(dep, pattern) for pattern in forbidden):
                errors.append(f"{package} must not depend on {dep}")
    for error in errors:
        print(error, file=sys.stderr)
    if errors:
        return 1
    print(f"Dependency boundaries hold for {checked} package(s)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
