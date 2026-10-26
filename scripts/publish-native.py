#!/usr/bin/env python3
"""Publish checked native assets without replacing an existing release."""
import json
import os
import re
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


def release_notes(message: str, release: str) -> str:
    """A stable release's notes are its release commit message, without the
    `Release vX:` prefix on the summary line."""
    subject, _, body = message.strip().partition("\n")
    subject = re.sub(rf"^Release {re.escape(release)}:\s*", "", subject).strip()
    return "\n\n".join(part for part in (subject, body.strip()) if part)


footer = (f"Native wt binaries for macOS and Linux, built from `{build}`.\n\n"
          "Downloads are verified against `wt-release.json` and `SHA256SUMS`.\n")
with tempfile.TemporaryDirectory(prefix="wt-release-") as scratch:
    notes = Path(scratch) / "notes.md"
    text = footer
    if not prerelease:
        message = subprocess.run(["git", "log", "-1", "--format=%B", build], check=True,
                                 capture_output=True, text=True).stdout
        if summary := release_notes(message, release):
            text = f"{summary}\n\n---\n{footer}"
    notes.write_text(text)
    command = ["gh", "release", "create", release, "--target", build, "--title", f"wt {release}",
               "--notes-file", str(notes)]
    if prerelease:
        command += ["--prerelease", "--latest=false"]
    command += assets
    # No --clobber or edit fallback: a published identity is immutable.
    subprocess.run(command, check=True)
