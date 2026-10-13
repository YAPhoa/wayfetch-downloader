"""Regression tests for check_dependency_boundaries.py.

Each test builds a small throwaway Cargo workspace with stub crates named
after forbidden dependencies and runs the check against it.
Run with: python3 -m unittest discover -s scripts
"""

from pathlib import Path
import subprocess
import sys
import tempfile
import textwrap
import unittest

SCRIPT = Path(__file__).resolve().parent / "check_dependency_boundaries.py"


def write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(textwrap.dedent(text))


class Workspace:
    """A workspace with stub domain, protocol, and engine crates."""

    def __init__(self, root: Path) -> None:
        self.root = root
        write(root / "Cargo.toml", """
            [workspace]
            resolver = "3"
            members = ["domain", "protocol", "engine"]
        """)
        self.manifests = {}
        for name, deps in [
            ("domain", ""),
            ("protocol", 'wayfetch-domain = { path = "../domain" }'),
            ("engine", 'wayfetch-protocol = { path = "../protocol" }'),
        ]:
            self.manifests[name] = f"""
                [package]
                name = "wayfetch-{name}"
                version = "0.0.0"
                edition = "2024"

                [dependencies]
                {deps}
            """
            write(root / name / "src/lib.rs", "")
        for stub in ["axum", "tauri"]:
            write(root / "stubs" / stub / "Cargo.toml", f"""
                [package]
                name = "{stub}"
                version = "0.0.0"
                edition = "2024"
            """)
            write(root / "stubs" / stub / "src/lib.rs", "")

    def add(self, crate: str, section: str) -> None:
        self.manifests[crate] += textwrap.dedent(section)

    def check(self) -> subprocess.CompletedProcess:
        for name, manifest in self.manifests.items():
            write(self.root / name / "Cargo.toml", manifest)
        subprocess.run(
            ["cargo", "generate-lockfile", "--offline", "--quiet"],
            cwd=self.root, check=True,
        )
        return subprocess.run(
            [sys.executable, str(SCRIPT)],
            cwd=self.root, capture_output=True, text=True,
        )


class DependencyBoundaryTest(unittest.TestCase):
    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.ws = Workspace(Path(tmp.name))

    def assert_rejects(self, result, *messages: str) -> None:
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        for message in messages:
            self.assertIn(message, result.stderr)

    def test_clean_workspace_passes(self) -> None:
        result = self.ws.check()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("hold for 3 package(s)", result.stdout)

    def test_normal_dependency_is_rejected_downstream(self) -> None:
        self.ws.add("domain", 'axum = { path = "../stubs/axum" }\n')
        self.assert_rejects(
            self.ws.check(),
            "wayfetch-domain must not depend on axum",
            "wayfetch-protocol must not depend on axum",
            "wayfetch-engine must not depend on axum",
        )

    def test_optional_dependency_behind_feature_is_rejected(self) -> None:
        self.ws.add("protocol", """
            axum = { path = "../stubs/axum", optional = true }

            [features]
            http = ["dep:axum"]
        """)
        self.assert_rejects(
            self.ws.check(), "wayfetch-protocol must not depend on axum"
        )

    def test_target_specific_dependency_is_rejected(self) -> None:
        self.ws.add("engine", """
            [target.'cfg(windows)'.dependencies]
            tauri = { path = "../stubs/tauri" }
        """)
        self.assert_rejects(
            self.ws.check(), "wayfetch-engine must not depend on tauri"
        )

    def test_dev_dependency_is_allowed(self) -> None:
        self.ws.add("engine", """
            [dev-dependencies]
            axum = { path = "../stubs/axum" }
        """)
        result = self.ws.check()
        self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == "__main__":
    unittest.main()
