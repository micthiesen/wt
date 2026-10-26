#!/usr/bin/env python3
"""Set each tracked file's mtime to the time of the last commit that touched it.

Cargo decides whether a workspace crate is fresh by comparing its source files'
mtimes with the cached build output. A fresh checkout stamps every file with
the checkout time, so a restored cache of workspace crates would always look
stale. With commit times instead, files unchanged since the cache was built
are older than its output and are reused; changed files are newer and rebuild.
Needs full history (`fetch-depth: 0`). See docs/ci.md.
"""

from __future__ import annotations

import os
import subprocess
import sys


def commit_times(log: str) -> dict[str, int]:
    """Newest commit time per path from `git log --format=%x00%ct --name-only`."""
    times: dict[str, int] = {}
    stamp = None
    for line in log.splitlines():
        if line.startswith("\0"):
            stamp = int(line[1:])
        elif line and stamp is not None:
            times.setdefault(line, stamp)
    return times


def main() -> int:
    tracked = subprocess.run(["git", "ls-files", "-z"], check=True, capture_output=True,
                             text=True).stdout.split("\0")
    tracked = [path for path in tracked if path]
    log = subprocess.run(["git", "-c", "core.quotepath=off", "log", "--format=%x00%ct",
                          "--name-only", "--no-renames", "HEAD"],
                         check=True, capture_output=True, text=True).stdout
    times = commit_times(log)
    missing = [path for path in tracked if path not in times]
    if missing:
        # A shallow clone cannot date these files; fail rather than leave a
        # cache that silently never reuses anything.
        print(f"no commit time for {len(missing)} tracked files (shallow clone?): {missing[:5]}",
              file=sys.stderr)
        return 1
    for path in tracked:
        if os.path.lexists(path) and not os.path.islink(path):
            os.utime(path, (times[path], times[path]))
    print(f"restored commit mtimes for {len(tracked)} files")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
