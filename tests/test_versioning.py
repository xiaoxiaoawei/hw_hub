"""Offline release-version checks; no real repository files are changed."""

import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "bump-version.py"
SPEC = importlib.util.spec_from_file_location("bump_version", SCRIPT)
versioning = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(versioning)


class VersioningTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        (self.root / "Cargo.toml").write_text(
            '[package]\nname = "flashtool"\nversion = "1.2.3" # application\n\n'
            '[dependencies]\nhelper = "1.2.3"\n', encoding="utf-8"
        )
        (self.root / "Cargo.lock").write_text(
            'version = 4\n\n[[package]]\nname = "flashtool"\nversion = "1.2.3"\n\n'
            '[[package]]\nname = "flashtool"\nversion = "1.2.3"\nsource = "registry+https://example.invalid"\n\n'
            '[[package]]\nname = "helper"\nversion = "1.2.3"\n', encoding="utf-8"
        )
        (self.root / "CHANGELOG.md").write_text(
            '# Changelog\n\n## [2.0.0]\nMajor work.\n\n## [1.3.0]\nMinor work.\n\n## [1.2.3]\nInitial.\n', encoding="utf-8"
        )

    def snapshot(self):
        return {path.name: path.read_bytes() for path in self.root.iterdir() if path.is_file()}

    def test_multiple_updates_change_only_the_local_package(self):
        self.assertEqual(versioning.update(self.root, "1.3.0"), "1.3.0")
        self.assertEqual(versioning.update(self.root, None, check=True), "1.3.0")
        self.assertEqual(versioning.update(self.root, "2.0.0"), "2.0.0")
        lock = versioning.tomllib.loads((self.root / "Cargo.lock").read_text())
        self.assertEqual([item["version"] for item in lock["package"]], ["2.0.0", "1.2.3", "1.2.3"])
        manifest = (self.root / "Cargo.toml").read_text()
        self.assertIn('version = "2.0.0" # application', manifest)
        self.assertIn('helper = "1.2.3"', manifest)
        self.assertFalse(list(self.root.glob(".*.version-*")))

    def test_dry_run_and_check_leave_files_unchanged(self):
        before = self.snapshot()
        self.assertEqual(versioning.update(self.root, "1.3.0", dry_run=True), "1.3.0")
        self.assertEqual(versioning.update(self.root, "1.2.3", check=True), "1.2.3")
        self.assertEqual(before, self.snapshot())

    def test_windows_crlf_files_keep_their_line_endings(self):
        for path in self.root.iterdir():
            path.write_bytes(path.read_bytes().replace(b"\r\n", b"\n").replace(b"\n", b"\r\n"))
        versioning.update(self.root, "1.3.0")
        self.assertEqual(versioning.update(self.root, None, check=True), "1.3.0")
        for path in self.root.iterdir():
            data = path.read_bytes()
            self.assertIn(b"\r\n", data)
            self.assertNotIn(b"\n", data.replace(b"\r\n", b""))

    def test_invalid_duplicate_and_lower_versions_are_rejected(self):
        before = self.snapshot()
        for value in ("v1.3.0", "01.3.0", "1.3", "1.3.0-beta.1", "1.2.3", "1.2.2"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                versioning.update(self.root, value)
        self.assertEqual(before, self.snapshot())

    def test_missing_and_duplicate_release_notes_are_rejected(self):
        with self.assertRaisesRegex(ValueError, "Write release notes"):
            versioning.update(self.root, "1.4.0")
        with (self.root / "CHANGELOG.md").open("a") as stream:
            stream.write("\n## [1.3.0]\nDuplicate.\n")
        before = self.snapshot()
        with self.assertRaisesRegex(ValueError, "duplicate release headings"):
            versioning.update(self.root, "1.3.0")
        self.assertEqual(before, self.snapshot())

    def test_inconsistent_lockfile_and_expected_version_fail_without_writes(self):
        before = self.snapshot()
        with self.assertRaisesRegex(ValueError, "Expected version"):
            versioning.update(self.root, "1.3.0", check=True)
        self.assertEqual(before, self.snapshot())
        lock = self.root / "Cargo.lock"
        lock.write_text(lock.read_text().replace('version = "1.2.3"', 'version = "1.2.4"', 1))
        before = self.snapshot()
        with self.assertRaisesRegex(ValueError, "different versions"):
            versioning.update(self.root, "1.3.0")
        self.assertEqual(before, self.snapshot())

    def test_failed_second_replacement_rolls_back_the_first(self):
        before = self.snapshot()
        replace = versioning.os.replace
        calls = 0

        def fail_once(source, destination):
            nonlocal calls
            calls += 1
            if calls == 2:
                raise OSError("simulated lockfile replace failure")
            return replace(source, destination)

        with mock.patch.object(versioning.os, "replace", side_effect=fail_once):
            with self.assertRaisesRegex(OSError, "simulated"):
                versioning.update(self.root, "1.3.0")
        self.assertEqual(before, self.snapshot())
        self.assertFalse(list(self.root.glob(".*.version-*")))

    def test_cli_checks_and_dry_runs_use_only_the_fixture(self):
        before = self.snapshot()
        for arguments in (("--check",), ("1.3.0", "--dry-run")):
            result = subprocess.run(
                [sys.executable, str(SCRIPT), "--root", str(self.root), *arguments],
                capture_output=True, text=True, check=False
            )
            self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(before, self.snapshot())


if __name__ == "__main__":
    unittest.main()
