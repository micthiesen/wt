#!/usr/bin/env python3
"""Exercise the release artifact trust boundary using isolated payloads."""
import argparse
import importlib.util
import json
from pathlib import Path
import tarfile
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("packaging", Path(__file__).with_name("package-native.py"))
packaging = importlib.util.module_from_spec(spec)
spec.loader.exec_module(packaging)
SHA = "a" * 40


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="wt-package-test-")
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        (self.bin / "wt-launcher").write_bytes(b"isolated launcher payload")

    def args(self, target):
        executable = self.bin / "wt"
        executable.write_text(f"#!/bin/sh\nprintf '%s\\n' 'wt-build-id:{SHA}:{target}'\n")
        executable.chmod(0o755)
        return argparse.Namespace(release="rust-test-fixture", build_id=SHA, target=target,
                                  bin_dir=self.bin, output=self.root / "dist")

    def test_complete_release_reproducible_archive_and_verified_manifest(self):
        for target in sorted(packaging.TARGETS):
            args = self.args(target)
            packaging.package(args)
            archive = next(args.output.glob(f"*-{target}.tar.gz"))
            before = archive.read_bytes()
            packaging.package(args)
            self.assertEqual(before, archive.read_bytes())
            with tarfile.open(archive) as tar:
                self.assertEqual([member.name.rsplit("/", 1)[-1] for member in tar],
                                 ["wt", "wt-launcher", "wt-build-info.json"])
                info = json.load(tar.extractfile(tar.getmembers()[-1]))
                self.assertEqual(info["build_id"], SHA)
                self.assertEqual(info["target"], target)
        args.installer = self.root / "install-source.sh"
        args.installer.write_text("#!/bin/sh\necho fixture\n")
        packaging.manifest(args)
        result = json.loads((args.output / "wt-release.json").read_text())
        self.assertEqual(len(result["artifacts"]), 4)
        installer = args.output / "install.sh"
        self.assertEqual(installer.read_bytes(), args.installer.read_bytes())
        self.assertIn(f"{packaging.digest(installer)}  install.sh\n",
                      (args.output / "SHA256SUMS").read_text())

    def test_missing_platform_and_changed_archive_never_publish_manifest(self):
        args = self.args("aarch64-apple-darwin")
        packaging.package(args)
        with self.assertRaisesRegex(ValueError, "incomplete"):
            packaging.manifest(args)
        archive = next(args.output.glob("*.tar.gz"))
        archive.write_bytes(archive.read_bytes() + b"changed")
        with self.assertRaisesRegex(ValueError, "no longer matches"):
            packaging.manifest(args)
        self.assertFalse((args.output / "wt-release.json").exists())

    def test_binary_with_wrong_embedded_identity_is_rejected(self):
        args = self.args("aarch64-apple-darwin")
        args.target = "x86_64-apple-darwin"
        with self.assertRaisesRegex(ValueError, "disagrees"):
            packaging.package(args)
        self.assertFalse(args.output.exists())

    def test_release_metadata_cannot_escape_output_directory(self):
        args = self.args("aarch64-apple-darwin")
        args.release = "../outside"
        with self.assertRaises(ValueError):
            packaging.package(args)
        args.release = "valid"
        args.build_id = "shortsha"
        with self.assertRaises(ValueError):
            packaging.package(args)


if __name__ == "__main__":
    unittest.main()
