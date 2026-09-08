"""Security boundaries and reproducibility checks for the public export tool.

Run with: python -m unittest discover -s scripts -p test_export_open_source.py
"""

from __future__ import annotations

import hashlib
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock

import export_open_source as exporter


class ExportOpenSourceTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.base = Path(self.temporary.name)
        self.root = self.base / "workspace"
        self.root.mkdir()
        self.output = self.base / "public"

    def write(self, relative_path, content="public content\n"):
        path = self.root / relative_path
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8", newline="\n")
        return path

    def manifest(self, *entries):
        self.write(exporter.MANIFEST_NAME, "\n".join(entries) + "\n")

    def assert_rejected(self, *entries):
        self.manifest(*entries)
        with self.assertRaises(exporter.ExportError):
            exporter.validate_sources(self.root)

    def test_rejects_traversal_absolute_and_nonportable_paths(self):
        for value in (
            "../outside.txt", "src/../../outside.txt", "/tmp/outside.txt",
            "C:/outside.txt", "C:outside.txt", "\\\\server\\share\\file.txt",
            "\\outside.txt", "src\\..\\file.txt", "./README.md",
            "src//main.rs", "src/*.rs", "src/file?.rs", "src/[a-z].rs",
            "src/NUL.txt", "src/file. /main.rs", "src/file:stream",
        ):
            with self.subTest(value=value):
                self.assert_rejected(value)

    def test_rejects_forbidden_directories_case_insensitively(self):
        for directory in exporter.FORBIDDEN_DIRECTORIES:
            with self.subTest(directory=directory):
                self.assert_rejected(f"nested/{directory.upper()}/source.txt")

    def test_rejects_glob_syntax_even_when_a_literal_file_exists(self):
        for value in ("src/[a-z].rs", "src/{a,b}.rs"):
            with self.subTest(value=value):
                self.write(value)
                self.assert_rejected(value)

    def test_rejects_binary_model_archive_and_sensitive_artifacts(self):
        for name in (
            "katago.EXE", "engine.DLL", "model.bin", "sources.zip", "game.sgf",
            "model.bin.txt", "weights.onnx", "weights.safetensors", ".env",
            ".env.production", "private.key", "key.pem", "id_rsa", "credentials.json",
        ):
            with self.subTest(name=name):
                self.write(name)
                self.assert_rejected(name)

    def test_rejects_duplicates_including_case_differences(self):
        self.write("README.md")
        self.assert_rejected("README.md", "README.md")
        self.assert_rejected("README.md", "readme.MD")

    def test_rejects_empty_missing_directory_and_reserved_checksum_inputs(self):
        for entries in ((), ("missing.rs",), ("src",), ("SHA256SUMS",)):
            with self.subTest(entries=entries):
                (self.root / "src").mkdir(exist_ok=True)
                self.assert_rejected(*entries)

    def test_rejects_binary_content_without_binary_extension(self):
        (self.root / "data.txt").write_bytes(b"hello\x00world")
        self.assert_rejected("data.txt")
        (self.root / "data.txt").write_bytes(b"\xff\xfe")
        self.assert_rejected("data.txt")

    def make_symlink_or_skip(self, path, target, is_directory=False):
        try:
            path.symlink_to(target, target_is_directory=is_directory)
        except OSError as error:
            self.skipTest(f"This host cannot create symlinks: {error}")

    def test_rejects_symlink_file(self):
        outside = self.base / "outside.txt"
        outside.write_text("private", encoding="utf-8")
        self.make_symlink_or_skip(self.root / "linked.txt", outside)
        self.assert_rejected("linked.txt")

    def test_rejects_symlink_directory_traversal(self):
        outside = self.base / "outside"
        outside.mkdir()
        (outside / "secret.txt").write_text("private", encoding="utf-8")
        self.make_symlink_or_skip(self.root / "linked", outside, True)
        self.assert_rejected("linked/secret.txt")

    def test_rejects_symlink_manifest(self):
        outside = self.base / "manifest.txt"
        outside.write_text("README.md\n", encoding="utf-8")
        self.write("README.md")
        self.make_symlink_or_skip(self.root / exporter.MANIFEST_NAME, outside)
        with self.assertRaises(exporter.ExportError):
            exporter.validate_sources(self.root)

    def test_rejects_windows_reparse_point_attributes(self):
        fake_stat = mock.Mock(st_mode=0o040755, st_file_attributes=0x400)
        with mock.patch.object(Path, "lstat", return_value=fake_stat):
            with self.assertRaisesRegex(exporter.ExportError, "reparse"):
                exporter._reject_links(self.root / "junction" / "file.txt")

    @unittest.skipUnless(os.name == "nt", "Windows junction behavior")
    def test_rejects_real_windows_junction_for_source_and_output(self):
        outside = self.base / "outside"
        outside.mkdir()
        (outside / "source.txt").write_text("outside", encoding="utf-8")
        junction = self.root / "junction"
        result = subprocess.run(
            ["cmd.exe", "/d", "/c", "mklink", "/J", str(junction), str(outside)],
            capture_output=True,
            check=False,
        )
        if result.returncode != 0:
            self.skipTest("This Windows host cannot create a test junction.")
        try:
            self.assert_rejected("junction/source.txt")
            self.write("README.md")
            self.manifest("README.md")
            with self.assertRaisesRegex(exporter.ExportError, "reparse"):
                exporter.export_sources(self.root, junction / "nested" / "public")
            self.assertFalse((outside / "nested").exists())
            self.assertEqual((outside / "source.txt").read_text(encoding="utf-8"), "outside")
        finally:
            # rmdir removes the junction itself, without traversing its target.
            junction.rmdir()

    def test_existing_output_is_never_modified(self):
        self.manifest("README.md")
        self.write("README.md")
        self.output.mkdir()
        sentinel = self.output / "important.txt"
        sentinel.write_bytes(b"keep exactly")
        with self.assertRaisesRegex(exporter.ExportError, "already exists"):
            exporter.export_sources(self.root, self.output)
        self.assertEqual(sentinel.read_bytes(), b"keep exactly")
        self.assertEqual(list(self.output.iterdir()), [sentinel])

    def test_rejects_output_under_symlink_parent(self):
        outside = self.base / "outside"
        outside.mkdir()
        linked = self.base / "linked"
        self.make_symlink_or_skip(linked, outside, True)
        self.write("README.md")
        self.manifest("README.md")
        with self.assertRaises(exporter.ExportError):
            exporter.export_sources(self.root, linked / "public")
        self.assertEqual(list(outside.iterdir()), [])

    def test_creates_missing_output_parents_after_validation(self):
        self.write("README.md")
        self.manifest("README.md")
        output = self.base / "new-parent" / "nested" / "public"
        exporter.export_sources(self.root, output)
        self.assertEqual((output / "README.md").read_bytes(), (self.root / "README.md").read_bytes())

    def test_invalid_manifest_does_not_create_output_parents(self):
        self.manifest("missing.rs")
        output = self.base / "new-parent" / "nested" / "public"
        with self.assertRaises(exporter.ExportError):
            exporter.export_sources(self.root, output)
        self.assertFalse((self.base / "new-parent").exists())

    def test_exact_allowlist_contents_and_reproducible_checksums(self):
        readme = self.write("README.md", "公开服务端\n")
        source = self.write("src/main.rs", "fn main() {}\n")
        self.write("private.txt", "not listed")
        self.write("KataGo/source.cpp", "not listed")
        self.manifest("# Explicit public files", "", "README.md", "src/main.rs")
        snapshots = exporter.export_sources(self.root, self.output)
        self.assertEqual([item.relative_path for item in snapshots], ["README.md", "src/main.rs"])
        actual = {path.relative_to(self.output).as_posix() for path in self.output.rglob("*") if path.is_file()}
        self.assertEqual(actual, {"README.md", "src/main.rs", "SHA256SUMS"})
        for original in (readme, source):
            relative = original.relative_to(self.root)
            self.assertEqual((self.output / relative).read_bytes(), original.read_bytes())
        checksums = (self.output / "SHA256SUMS").read_text(encoding="utf-8").splitlines()
        self.assertEqual(len(checksums), 2)
        for line in checksums:
            digest, relative = line.split("  ", 1)
            self.assertEqual(digest, hashlib.sha256((self.output / relative).read_bytes()).hexdigest())
        second = self.base / "public-again"
        exporter.export_sources(self.root, second)
        self.assertEqual((self.output / "SHA256SUMS").read_bytes(), (second / "SHA256SUMS").read_bytes())

    def test_credential_errors_report_locations_without_values(self):
        value = "ghp_" + "0123456789abcdef" * 3
        self.write("config.txt", f"safe line\nGITHUB_TOKEN={value}\n")
        self.manifest("config.txt")
        with self.assertRaises(exporter.ExportError) as caught:
            exporter.export_sources(self.root, self.output)
        message = str(caught.exception)
        self.assertIn("config.txt:2:", message)
        self.assertIn("GitHub token", message)
        self.assertNotIn(value, message)
        self.assertFalse(self.output.exists())

    def test_detects_literal_credentials_and_private_keys(self):
        values = (
            'api_' + 'key = "non-placeholder-value"',
            "SERVICE_PASSWORD=non-placeholder-value",
            "-----BEGIN " + "RSA PRIVATE KEY-----",
            'url = "https://' + 'alice:non-placeholder-value@example.org"',
        )
        for value in values:
            with self.subTest(value=value):
                self.write("config.txt", value + "\n")
                self.assert_rejected("config.txt")

    def test_accepts_explicit_placeholder_credentials(self):
        self.write(".env.example", "API_KEY=your-api-key\nSERVICE_PASSWORD=${SERVICE_PASSWORD}\n")
        self.write("example.toml", 'api_key = "<API_KEY>"\npassword = "change-me"\ntoken = ""\n')
        self.manifest(".env.example", "example.toml")
        self.assertEqual(len(exporter.validate_sources(self.root)), 2)

    def test_exporter_and_test_fixtures_pass_their_own_credential_scan(self):
        for path in (Path(exporter.__file__), Path(__file__)):
            with self.subTest(path=path.name):
                self.assertEqual(exporter._scan_secrets(path.name, path.read_bytes()), [])

    def test_failed_export_keeps_partial_output_for_inspection(self):
        self.write("README.md")
        self.manifest("README.md")
        real_open = Path.open

        def fail_checksum(path, *args, **kwargs):
            if path.name == "SHA256SUMS":
                raise OSError("simulated storage error")
            return real_open(path, *args, **kwargs)

        with mock.patch.object(Path, "open", fail_checksum):
            with self.assertRaisesRegex(exporter.ExportError, "nothing was deleted"):
                exporter.export_sources(self.root, self.output)
        self.assertTrue((self.output / "README.md").is_file())


if __name__ == "__main__":
    unittest.main()
