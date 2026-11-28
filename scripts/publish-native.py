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
assets += ["dist/wt-release.json", "dist/SHA256SUMS", "dist/install.sh"]
for asset in assets:
    if not Path(asset).is_file():
        raise SystemExit(f"required release asset missing: {asset}")
prerelease = os.environ["RELEASE_PRERELEASE"] == "true"


footer = (f"Native wt binaries for macOS and Linux, built from `{build}`.\n\n"
          "Downloads are verified against `wt-release.json` and `SHA256SUMS`.\n")
with tempfile.TemporaryDirectory(prefix="wt-release-") as scratch:
    notes = Path(scratch) / "notes.md"
    # Written by release-notes.py from every commit since the previous release.
    # Missing or unreadable notes never stop a publication.
    try:
        drafted = json.loads(Path(os.environ["RELEASE_NOTES"]).read_text())
        title, summary = str(drafted["title"]).strip(), str(drafted["notes"]).strip()
    except (KeyError, OSError, ValueError, TypeError):
        title = summary = ""
    notes.write_text(f"{summary}\n\n---\n{footer}" if summary and not prerelease else footer)
    heading = f"wt {release}: {title}" if title and not prerelease else f"wt {release}"
    command = ["gh", "release", "create", release, "--target", build, "--title", heading,
               "--notes-file", str(notes)]
    if prerelease:
        command += ["--prerelease", "--latest=false"]
    command += assets
    # No --clobber or edit fallback: a published identity is immutable.
    subprocess.run(command, check=True)
