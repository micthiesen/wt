#!/usr/bin/env python3
"""Measure a real wt process in an isolated PTY and synthetic Git fleet.

Example: python3 scripts/perf-baseline.py --output /tmp/wt-baseline -- bun src/main.ts
All mutable paths, HOME, tmux state and repositories belong to this run. The
command is resolved before changing cwd. No test talks to GitHub or starts agents.
Results include waited process-tree CPU, terminal bytes, and wt's own latency log.
"""

import argparse
import errno
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import shutil
import signal
import struct
import subprocess
import sys
import termios
import time


def run(argv, cwd=None, env=None):
    return subprocess.run(argv, cwd=cwd, env=env, check=True, capture_output=True).stdout


def fixture(root, rows):
    if root.exists():
        raise SystemExit(f"Refusing to overwrite existing benchmark directory: {root}")
    root.mkdir(parents=True)
    main = root / "main"
    origin = root / "origin.git"
    worktrees = root / "worktrees"
    home = root / "home"
    worktrees.mkdir()
    home.mkdir()
    git_env = dict(os.environ, HOME=str(home), GIT_CONFIG_NOSYSTEM="1")
    for key in ("GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_CONFIG_GLOBAL"):
        git_env.pop(key, None)
    run(["git", "init", "--bare", "--initial-branch=main", str(origin)], env=git_env)
    run(["git", "clone", str(origin), str(main)], env=git_env)
    run(["git", "config", "user.name", "wt performance fixture"], main, git_env)
    run(["git", "config", "user.email", "wt-fixture@example.invalid"], main, git_env)
    (main / "example.txt").write_text("baseline\n")
    run(["git", "add", "."], main, git_env)
    run(["git", "commit", "-m", "Fixture baseline"], main, git_env)
    run(["git", "push", "origin", "main"], main, git_env)
    for n in range(rows):
        path = worktrees / f"bench-{n:03}"
        run(["git", "worktree", "add", "-b", f"bench/bench-{n:03}", str(path)], main, git_env)
        (path / "example.txt").write_text(f"worktree {n}\n")
        run(["git", "add", "."], path, git_env)
        run(["git", "commit", "-m", f"Fixture work {n}"], path, git_env)
        if n % 3 == 0:
            (path / "dirty.txt").write_text("dirty fixture\n")
    config = root / "config.toml"
    # JSON string escaping is also valid for these TOML basic string values.
    config.write_text("\n".join([
        "[paths]", f"main_clone = {json.dumps(str(main))}",
        f"worktree_root = {json.dumps(str(worktrees))}",
        f"cache_db = {json.dumps(str(root / 'cache/cache.sqlite'))}",
        f"state_db = {json.dumps(str(root / 'state/wt.sqlite'))}",
        '[branch]', 'prefix = "bench"', 'base = "main"',
        '[naming]', 'auto_rename = false',
    ]) + "\n")
    return main, home, config


def measure(root, command, main, home, config, scenario, seconds):
    socket = f"wt-perf-{os.getpid()}-{scenario}"
    env = dict(os.environ, HOME=str(home), TERM="xterm-256color", COLORTERM="truecolor",
               WT_CONFIG=str(config), WT_TMUX_SOCKET=socket, WT_UPDATE="off",
               WT_SKILLS="off", WT_GITHUB="off", WT_AUTOMATIONS="off", WT_PERF="1")
    for key in ("BUN_INSPECT", "WT_REPO_CONFIG", "WT_AGENT", "WT_REPO_ID",
                "XDG_CONFIG_HOME", "XDG_STATE_HOME", "XDG_CACHE_HOME"):
        env.pop(key, None)
    pid, master = pty.fork()
    if pid == 0:
        os.chdir(main)
        os.execvpe(command[0], command, env)
    fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 50, 180, 0, 0))
    os.set_blocking(master, False)
    start = time.monotonic()
    last_key = start
    first_output = None
    total_bytes = 0
    sent = 0
    quitting = False
    usage = None
    status = None
    capture = root / f"{scenario}.ansi"
    try:
        with capture.open("wb") as out:
            while True:
                now = time.monotonic()
                elapsed = now - start
                ready, _, _ = select.select([master], [], [], 0.01)
                if ready:
                    try:
                        data = os.read(master, 65536)
                    except OSError as error:
                        if error.errno == errno.EIO:
                            data = b""
                        else:
                            raise
                    if data:
                        first_output = first_output if first_output is not None else time.monotonic() - start
                        total_bytes += len(data)
                        out.write(data)
                        # Respond to terminal position/device queries so startup
                        # follows a real terminal's path rather than a timeout.
                        if b"\x1b[6n" in data:
                            os.write(master, b"\x1b[1;1R")
                        if b"\x1b[c" in data:
                            os.write(master, b"\x1b[?1;2c")
                if not quitting and elapsed >= seconds:
                    os.write(master, b"q")
                    quitting = True
                if not quitting and elapsed > 5:
                    interval = 0.08 if scenario == "navigation" else 5
                    if scenario != "idle" and now - last_key >= interval:
                        key = (b"j" if sent % 40 < 20 else b"k") if scenario == "navigation" else b"r"
                        os.write(master, key)
                        sent += 1
                        last_key = now
                done, status, usage = os.wait4(pid, os.WNOHANG)
                if done:
                    break
                if elapsed > seconds + 8:
                    raise RuntimeError("wt did not exit after q")
    finally:
        if usage is None:
            try:
                os.killpg(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            _, status, usage = os.wait4(pid, 0)
        os.close(master)
        subprocess.run(["tmux", "-L", socket, "kill-server"], capture_output=True)
    elapsed = time.monotonic() - start
    result = dict(scenario=scenario, elapsed_seconds=elapsed, first_output_seconds=first_output,
                  terminal_bytes=total_bytes, input_keys=sent, exit_code=os.waitstatus_to_exitcode(status),
                  process_tree_cpu_seconds=usage.ru_utime + usage.ru_stime,
                  process_tree_cpu_percent=100 * (usage.ru_utime + usage.ru_stime) / elapsed,
                  peak_rss_bytes=usage.ru_maxrss * (1 if sys.platform == "darwin" else 1024))
    (root / f"{scenario}.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result), flush=True)
    if result["exit_code"] != 0:
        raise RuntimeError(f"wt failed during {scenario}; inspect {capture}")
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--rows", type=int, default=24)
    parser.add_argument("--seconds", type=float, default=65)
    parser.add_argument("--scenarios", default="idle,navigation,refresh")
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        parser.error("provide the executable command after --")
    executable = shutil.which(command[0])
    if executable is None:
        parser.error(f"executable not found: {command[0]}")
    command[0] = executable
    command[1:] = [str(Path(arg).resolve()) if Path(arg).is_file() else arg for arg in command[1:]]
    scenarios = args.scenarios.split(",")
    if any(s not in ("idle", "navigation", "refresh") for s in scenarios):
        parser.error("scenarios: idle,navigation,refresh")
    root = args.output.resolve()
    main_clone, home, config = fixture(root, args.rows)
    results = [measure(root, command, main_clone, home, config, s, args.seconds) for s in scenarios]
    (root / "results.json").write_text(json.dumps(dict(
        command=command, rows=args.rows, platform=sys.platform, scenarios=results,
        notes=["CPU includes the waited process tree; intentionally detached daemons are excluded.",
               "first_output measures terminal activity, not a verified complete first frame.",
               "Navigation latency must come from wt's input-latency instrumentation in the isolated app log."],
    ), indent=2) + "\n")


if __name__ == "__main__":
    main()
