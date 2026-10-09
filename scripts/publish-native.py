#!/usr/bin/env python3
"""Publish checked native assets without replacing an existing release."""
import json
import os
from pathlib import Path
import subprocess
import tempfile

manifest = json.loads(Path("dist/wt-release.json").read_text())
release = os.environ["RELEASE_VERSION"]
build = os.environ["WT_BUILD_ID"]
if manifest["release_version"] != release or manifest["build_id"] != build:
    raise SystemExit("release manifest identity changed before publication")
assets = [str(Path("dist") / entry["filename"]) for entry in manifest["artifacts"]]
assets += ["dist/wt-release.json", "dist/SHA256SUMS"]
with tempfile.TemporaryDirectory(prefix="wt-release-") as scratch:
    notes = Path(scratch) / "notes.md"
    notes.write_text(f"Native wt binaries for macOS and Linux, built from `{build}`.\n\n"
                     "Downloads are verified against `wt-release.json` and `SHA256SUMS`.\n")
    command = ["gh", "release", "create", release, "--target", build, "--title", f"wt {release}",
               "--notes-file", str(notes)]
    if os.environ["RELEASE_PRERELEASE"] == "true":
        command += ["--prerelease", "--latest=false"]
    command += assets
    # No --clobber or edit fallback: a published identity is immutable.
    subprocess.run(command, check=True)
