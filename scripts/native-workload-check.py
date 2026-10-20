#!/usr/bin/env python3
"""Verify configured CPU and RAM limits with real Linux user scopes."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import shlex
import signal
import subprocess
import sys
import tempfile
import time
import uuid


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if platform.system() != "Linux":
        raise SystemExit("This check requires Linux with a systemd user manager.")
    binary = args.binary.resolve()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    runtime = Path(f"/run/user/{os.getuid()}")
    if not (runtime / "bus").exists():
        raise SystemExit("Start the systemd user manager before running this check.")
    group = "fixture_" + uuid.uuid4().hex
    slice_name = f"wtworkloads-{group}.slice"
    environment = {**os.environ, "XDG_RUNTIME_DIR": str(runtime),
                   "DBUS_SESSION_BUS_ADDRESS": f"unix:path={runtime}/bus"}

    def run(*argv, check=True, timeout=30, env=None, cwd=None):
        result = subprocess.run([str(arg) for arg in argv], env=env or environment,
                                cwd=cwd, text=True, capture_output=True, timeout=timeout)
        if check and result.returncode:
            raise AssertionError(f"{argv}: exit {result.returncode}\n{result.stdout}\n{result.stderr}")
        return result

    # This probe fails before any repository, user home, or live tmux state is used.
    run("systemctl", "--user", "show", "--property=Version")
    with tempfile.TemporaryDirectory(prefix="wt-workload-") as temporary:
        root = Path(temporary)
        home = root / "home"
        repo = root / "repo"
        trees = root / "trees"
        home.mkdir()
        trees.mkdir()
        run("git", "init", "--initial-branch=main", repo)
        run("git", "-C", repo, "-c", "user.name=Fixture", "-c", "user.email=fixture@invalid",
            "commit", "--allow-empty", "-m", "Fixture")
        run("git", "-C", repo, "worktree", "add", "-b", "one", trees / "one")
        report = output / "dev-report.json"
        report.unlink(missing_ok=True)
        worker = root / "worker.py"
        worker.write_text('''import json, os, pathlib, subprocess, sys, time
path = next(line[3:] for line in pathlib.Path('/proc/self/cgroup').read_text().splitlines() if line.startswith('0::'))
group = pathlib.Path('/sys/fs/cgroup') / path.lstrip('/')
def stats():
    return dict(line.split() for line in (group / 'cpu.stat').read_text().splitlines())
before = stats()
started = time.monotonic()
while time.monotonic() - started < 2.5:
    pass
after = stats()
child = subprocess.check_output([sys.executable, '-c', "import pathlib; print(pathlib.Path('/proc/self/cgroup').read_text(), end='')"], text=True)
result = {'membership': path, 'child_membership': child, 'cpu': (group / 'cpu.max').read_text(),
          'memory': (group / 'memory.max').read_text(), 'shared_cpu': (group.parent / 'cpu.max').read_text(),
          'shared_memory': (group.parent / 'memory.max').read_text(), 'shared_high': (group.parent / 'memory.high').read_text(),
          'throttled': int(after['nr_throttled']) - int(before['nr_throttled'])}
pathlib.Path(sys.argv[1]).write_text(json.dumps(result))
time.sleep(120)
''')
        config = root / "config.toml"
        quote = json.dumps
        config.write_text(f'''[paths]
main_clone = {quote(str(repo))}
worktree_root = {quote(str(trees))}
state_db = {quote(str(root / 'state.sqlite'))}
cache_db = {quote(str(root / 'cache.sqlite'))}
[branch]
prefix = "fixture"
[tmux]
socket = "{group}"
[resources.linux]
group = "{group}"
[resources.linux.shared]
cpu_quota_percent = 50
memory_high = "384M"
memory_max = "512M"
[resources.linux.job]
cpu_quota_percent = 25
memory_max = "256M"
[dev_server]
command = {quote(shlex.join([sys.executable, str(worker), str(report)]))}
''')
        environment.update({"HOME": str(home), "WT_CONFIG": str(config), "WT_REPO_CONFIG": "",
                            "WT_TMUX_SOCKET": group, "WT_UPDATE": "off", "WT_GITHUB": "off"})
        helper = [binary, "_workload", "--group", group, "--shared-cpu", "50",
                  "--shared-memory-high", str(384 * 1024**2),
                  "--shared-memory-max", str(512 * 1024**2)]
        try:
            run(binary, "dev", "start", "one", cwd=repo)
            deadline = time.monotonic() + 20
            while not report.exists() and time.monotonic() < deadline:
                time.sleep(0.1)
            assert report.exists(), run(binary, "dev", "logs", "one", cwd=repo).stdout
            data = json.loads(report.read_text())
            assert f"/{slice_name}/wtjob-" in data["membership"], data
            assert "0::" + data["membership"] in data["child_membership"], data
            assert data["cpu"].strip() == "25000 100000", data
            assert data["shared_cpu"].strip() == "50000 100000", data
            assert int(data["memory"]) == 256 * 1024**2, data
            assert int(data["shared_memory"]) == 512 * 1024**2, data
            assert int(data["shared_high"]) == 384 * 1024**2, data
            assert data["throttled"] > 0, data

            identity = run(*helper, "--", "python3", "-c",
                           "import os,pathlib; p=pathlib.Path('/proc/self/cgroup').read_text(); "
                           "assert '/wtjob-'+str(os.getpid())+'-' in p; print('same PID')")
            assert identity.stdout.strip() == "same PID", identity
            terminated = run(*helper, "--", "/bin/sh", "-c", "kill -TERM $$", check=False)
            assert terminated.returncode == -signal.SIGTERM, terminated
            memory = run(*helper, "--job-memory-max", str(64 * 1024**2), "--", "python3", "-c",
                         "data=[]\nfor i in range(160): data.append(bytearray(1024*1024))",
                         check=False)
            assert memory.returncode == -signal.SIGKILL, memory
            marker = root / "must-not-exist"
            missing_tools = run(*helper, "--", "/usr/bin/touch", marker, check=False,
                                env={**environment, "PATH": "/missing-tools"})
            assert missing_tools.returncode != 0 and not marker.exists(), missing_tools
            digest = hashlib.sha256()
            with binary.open("rb") as stream:
                for chunk in iter(lambda: stream.read(1024 * 1024), b""):
                    digest.update(chunk)
            (output / "config.toml").write_text(config.read_text())
            data.update({"binary_sha256": digest.hexdigest(), "identity": identity.stdout.strip(), "signal_exit": terminated.returncode,
                         "memory_exit": memory.returncode, "missing_tools_exit": missing_tools.returncode,
                         "binary": str(binary), "kernel": platform.release(), "group": group})
            (output / "report.json").write_text(json.dumps(data, indent=2) + "\n")
            print(json.dumps(data, indent=2), flush=True)
        finally:
            run(binary, "dev", "stop", "one", check=False, cwd=repo)
            run("tmux", "-L", group, "kill-server", check=False)
            run("systemctl", "--user", "stop", slice_name, check=False)
            (runtime / "systemd/user" / slice_name).unlink(missing_ok=True)
            shutil.rmtree(runtime / "systemd/user.control" / (slice_name + ".d"), ignore_errors=True)
            run("systemctl", "--user", "daemon-reload", check=False)
            (runtime / "wt-resources" / (group + ".lock")).unlink(missing_ok=True)


if __name__ == "__main__":
    main()
