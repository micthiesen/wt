#!/usr/bin/env python3
"""Resolve a release identity without evaluating workflow input as shell code."""
import os
from pathlib import Path
import re
import subprocess

sha = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
timestamp = subprocess.check_output(["git", "show", "-s", "--format=%ct", "HEAD"], text=True).strip()
ref = os.environ["GITHUB_REF"]
event = os.environ["GITHUB_EVENT_NAME"]
requested = os.environ.get("INPUT_RELEASE_TAG", "").strip()
if ref.startswith("refs/tags/"):
    release = ref.removeprefix("refs/tags/")
elif requested:
    release = requested
elif ref == "refs/heads/main":
    release = f"preview-{sha}"
else:
    release = f"rust-test-{sha[:12]}-{os.environ['GITHUB_RUN_ID']}"
if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._+-]*", release):
    raise SystemExit("release tag contains invalid characters")
if event == "workflow_dispatch" and ref != "refs/heads/main" and not release.startswith("rust-test-"):
    raise SystemExit("test-branch releases must use the rust-test- prefix")
stable = bool(re.fullmatch(r"v\d+\.\d+\.\d+", release))
publish = event == "push" or os.environ.get("INPUT_PUBLISH", "false") == "true"
with Path(os.environ["GITHUB_OUTPUT"]).open("a") as output:
    for key, value in {"sha": sha, "release": release, "epoch": timestamp,
                       "prerelease": str(not stable).lower(), "publish": str(publish).lower()}.items():
        output.write(f"{key}={value}\n")
print(f"{'Stable' if stable else 'Prerelease'} {release} from {sha}; publish={publish}")
