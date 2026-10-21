"""Tests for updater_manifest.py.

Run with: python3 -m unittest discover -s scripts
"""

from datetime import datetime, timezone
from pathlib import Path
import tempfile
import unittest

from updater_manifest import manifest, platform

BASE = "https://github.com/YAPhoa/wayfetch-downloader/releases/download/v0.2.0"
NOW = datetime(2026, 10, 21, 12, 0, tzinfo=timezone.utc)


class ManifestTest(unittest.TestCase):
    def setUp(self) -> None:
        self.dir = tempfile.TemporaryDirectory()
        self.root = Path(self.dir.name)

    def tearDown(self) -> None:
        self.dir.cleanup()

    def package(self, name: str, signature: str | None = "c2lnbmF0dXJl\n") -> Path:
        path = self.root / name
        path.write_bytes(b"package")
        if signature is not None:
            (self.root / f"{name}.sig").write_text(signature)
        return path

    def test_each_package_is_listed_under_its_kind_and_machine(self) -> None:
        deb = self.package("wayfetch-downloader_0.2.0_amd64.deb")
        rpm = self.package("wayfetch-downloader-0.2.0-1.x86_64.rpm", "b3RoZXI=")
        self.assertEqual(
            manifest("v0.2.0", BASE + "/", [deb, rpm], NOW),
            {
                "version": "0.2.0",
                "pub_date": "2026-10-21T12:00:00Z",
                "platforms": {
                    "linux-x86_64-deb": {
                        "signature": "c2lnbmF0dXJl",
                        "url": f"{BASE}/wayfetch-downloader_0.2.0_amd64.deb",
                    },
                    "linux-x86_64-rpm": {
                        "signature": "b3RoZXI=",
                        "url": f"{BASE}/wayfetch-downloader-0.2.0-1.x86_64.rpm",
                    },
                },
            },
        )

    def test_names_are_read_for_the_machine(self) -> None:
        self.assertEqual(platform(Path("w_0.2.0_arm64.deb")), "linux-aarch64-deb")
        with self.assertRaisesRegex(ValueError, "not a .deb or .rpm"):
            platform(Path("w_0.2.0_amd64.AppImage"))
        with self.assertRaisesRegex(ValueError, "no machine architecture"):
            platform(Path("w_0.2.0.deb"))

    def test_an_unsigned_or_repeated_package_is_refused(self) -> None:
        unsigned = self.package("w_0.2.0_amd64.deb", signature=None)
        with self.assertRaisesRegex(ValueError, "no signature"):
            manifest("0.2.0", BASE, [unsigned], NOW)
        first = self.package("a_0.2.0_amd64.deb")
        second = self.package("b_0.2.0_amd64.deb")
        with self.assertRaisesRegex(ValueError, "a second package"):
            manifest("0.2.0", BASE, [first, second], NOW)
        with self.assertRaisesRegex(ValueError, "no packages"):
            manifest("0.2.0", BASE, [], NOW)


if __name__ == "__main__":
    unittest.main()
