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
import uuid

sys.dont_write_bytecode = True


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
    # Lifecycle cleanup may consult browser helpers even when no browser was
    # opened. Keep the benchmark away from the user's browser relay and tabs.
    tools = root / "fixture-bin"
    tools.mkdir()
    browser = tools / "browser-control"
    browser.write_text(
        "#!" + sys.executable + "\nimport json, sys\n"
        "if sys.argv[1:] == ['status', '--json']:\n"
        " print(json.dumps({'relay': {'running': False}, 'extension': {'sessions': []}}))\n"
        "else: raise SystemExit('unexpected browser command in benchmark')\n"
    )
    browser.chmod(0o700)
    ps = tools / "ps"
    ps.write_text(
        "#!" + sys.executable + "\nimport os, sys\n"
        "if sys.argv[1:] == ['-Aco', 'command']: print('COMMAND')\n"
        f"else: os.execv({shutil.which('ps')!r}, ['ps', *sys.argv[1:]])\n"
    )
    ps.chmod(0o700)
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


def measure(root, command, main, home, config, scenario, seconds, ui):
    socket = f"wt-perf-{os.getpid()}-{scenario}"
    tmux_tmp = root / f"tmux-{scenario}"
    tmux_tmp.mkdir(exist_ok=True)
    env = dict(os.environ, HOME=str(home), TERM="xterm-256color", COLORTERM="truecolor",
               WT_CONFIG=str(config), WT_TMUX_SOCKET=socket, WT_UPDATE="off",
               WT_SKILLS="off", WT_GITHUB="off", WT_AUTOMATIONS="off", WT_PERF="1",
               TMUX_TMPDIR=str(tmux_tmp))
    env['PATH'] = str(root / 'fixture-bin') + os.pathsep + env.get('PATH', '')
    for key in ("BUN_INSPECT", "WT_REPO_CONFIG", "WT_AGENT", "WT_REPO_ID",
                "XDG_CONFIG_HOME", "XDG_STATE_HOME", "XDG_CACHE_HOME", "TMUX", "TMUX_PANE",
                "CODEX_HOME", "BUN_OPTIONS", "NODE_OPTIONS"):
        env.pop(key, None)
    codex_path = None
    codex_slug = None
    codex_message = None
    if scenario == "codex_output":
        codex_slug = "bench-000"
        codex_message = f"post_warmup_{uuid.uuid4().hex[:12]}"
        session_id = str(uuid.uuid4())
        worktree = root / "worktrees" / codex_slug
        now = time.gmtime()
        codex_path = (
            home / ".codex" / "sessions" / time.strftime("%Y/%m", now)
            / time.strftime("%d", now) / f"rollout-{session_id}.jsonl"
        )
        codex_path.parent.mkdir(parents=True, exist_ok=True)
        header = {"type": "session_meta", "payload": {
            "id": session_id, "cwd": str(worktree), "originator": "codex-tui", "thread_source": "user"
        }}
        user = {"type": "event_msg", "timestamp": "2026-10-09T12:00:00Z", "payload": {
            "type": "user_message", "message": "synthetic fixture prompt"
        }}
        assistant = {"type": "event_msg", "timestamp": "2026-10-09T12:00:01Z", "payload": {
            "type": "agent_message", "message": "seed_output_ready"
        }}
        codex_path.write_text("\n".join(json.dumps(row) for row in (header, user, assistant)) + "\n")
        # The legacy TypeScript UI does not trust tmux's creation-time session
        # option as proof of the process running inside the pane. It recovers
        # the live Codex UUID by inspecting the pane PID with lsof for an open
        # ~/.codex/thread-writer-locks/<uuid>.lock file. Model that ownership
        # evidence with a tiny inert process that holds the exact lock open.
        lock_dir = home / ".codex" / "thread-writer-locks"
        lock_dir.mkdir(parents=True, exist_ok=True)
        writer_lock = lock_dir / f"{session_id}.lock"
        writer_lock.touch()
        holder = root / "hold-codex-writer-lock.py"
        holder.write_text(
            "import sys, time\n"
            "lock = open(sys.argv[1], 'rb')\n"
            "while True: time.sleep(60)\n"
        )
        try:
            run(["tmux", "-L", socket, "new-session", "-d", "-s", f"{codex_slug}-codex", "-c", str(worktree),
                 sys.executable, str(holder), str(writer_lock)], env=env)
            run(["tmux", "-L", socket, "set-option", "-t", f"{codex_slug}-codex", "@wt-harness-session-id", session_id], env=env)
            tmux_sessions = run(["tmux", "-L", socket, "list-sessions", "-F", "#{session_name}\t#{@wt-harness-session-id}"], env=env).decode(errors="replace")
            if (f"{codex_slug}-codex\t{session_id}" not in tmux_sessions
                    and f"{codex_slug}-codex_{session_id}" not in tmux_sessions):
                raise RuntimeError(f"Codex fixture is missing its exact live tmux identity: {tmux_sessions!r}")
            pane_pid = run(["tmux", "-L", socket, "list-panes", "-s", "-t", f"={codex_slug}-codex", "-F", "#{pane_pid}"], env=env).decode().strip()
            open_files = run(["lsof", "-nP", "-a", "-p", pane_pid, "-Fn"], env=env).decode(errors="replace")
            if str(writer_lock) not in open_files:
                raise RuntimeError("Codex fixture pane does not hold its writer lock; TS identity recovery cannot match the live rollout")
        except Exception:
            subprocess.run(["tmux", "-L", socket, "kill-server"], env=env, capture_output=True)
            raise
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
    driver_cpu_start = time.process_time()
    child_cpu = {"lifecycle": 0.0, "fixture_writer": 0.0}
    child_elapsed = {"lifecycle": 0.0}
    child = None
    child_started = None
    lifecycle_slug = f"bench-live-{os.getpid()}"
    lifecycle_stage = 0
    key_latency_ms = None
    key_output_floor = None
    codex_picker_opened = False
    codex_picker_selected = False
    codex_seed_seen_at = None
    codex_writer_started = False
    codex_writer_started_at = None
    codex_post_warmup_seen_at = None
    writer_usage = None
    writer_status = None
    writer_pid = None
    capture = root / f"{scenario}.ansi"
    capture_bytes = bytearray()
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
                        capture_bytes.extend(data)
                        out.write(data)
                        if key_latency_ms is None and key_output_floor is not None and total_bytes > key_output_floor:
                            key_latency_ms = (time.monotonic() - key_injected_at) * 1000
                        # Respond to terminal position/device queries so startup
                        # follows a real terminal's path rather than a timeout.
                        if b"\x1b[6n" in data:
                            os.write(master, b"\x1b[1;1R")
                        if b"\x1b[c" in data:
                            os.write(master, b"\x1b[?1;2c")
                captured = capture_bytes
                if scenario == "lifecycle" and lifecycle_stage == 0 and elapsed >= min(2.0, max(0.5, seconds * 0.2)):
                    log = (root / "lifecycle-new.log").open("wb")
                    child_started = time.monotonic()
                    child = subprocess.Popen(
                        command + ["new", "--no-install", "--no-open", lifecycle_slug], cwd=main,
                        env=env, stdout=log, stderr=subprocess.STDOUT,
                    )
                    child_log = log
                    lifecycle_stage = 1
                    key_output_floor = total_bytes
                    key_injected_at = time.monotonic()
                    os.write(master, b"j")
                    sent += 1
                elif scenario == "lifecycle" and lifecycle_stage == 2 and child is None:
                    created_path = root / "worktrees" / lifecycle_slug
                    if not created_path.exists():
                        raise RuntimeError(f"wt new succeeded without creating {created_path}; see lifecycle-new.log")
                    visible_names = (lifecycle_slug, lifecycle_slug.replace('-', ' '))
                    if any(name.encode().lower() in captured.lower() for name in visible_names):
                        log = (root / "lifecycle-rm.log").open("wb")
                        child_started = time.monotonic()
                        child = subprocess.Popen(
                            command + ["rm", lifecycle_slug, "--yes", "--force"], cwd=main,
                            env=env, stdout=log, stderr=subprocess.STDOUT,
                        )
                        child_log = log
                        lifecycle_stage = 3
                        key_output_floor = total_bytes
                        key_injected_at = time.monotonic()
                        os.write(master, b"k")
                        sent += 1
                if (scenario == "codex_output" and not codex_picker_opened and codex_slug.encode() in captured
                        and (ui != 'legacy' or elapsed >= 5)):
                    # Select the selected-worktree output stream. This waits
                    # for the live tmux identity and rollout to reach the row.
                    os.write(master, b"'")
                    sent += 1
                    codex_picker_opened = True
                    key_output_floor = total_bytes
                    key_injected_at = time.monotonic()
                if scenario == "codex_output" and codex_picker_opened and not codex_picker_selected and b"Output source" in captured:
                    os.write(master, b"jj\r")
                    sent += 3
                    codex_picker_selected = True
                elif (scenario == "codex_output" and codex_picker_opened and not codex_picker_selected
                      and ui == 'legacy' and b'outputs' in captured and b'codex' in captured.lower()):
                    # Legacy sorts the one live Codex session before both event
                    # feeds. Native exposes selected-worktree output separately.
                    os.write(master, b'1')
                    sent += 1
                    codex_picker_selected = True
                if scenario == "codex_output" and codex_picker_opened and codex_seed_seen_at is None and b"seed_output_ready" in captured:
                    codex_seed_seen_at = time.monotonic()
                if (scenario == "codex_output" and codex_seed_seen_at is not None and not codex_writer_started
                        and elapsed >= 10.0):
                    # Sustain output after the initial tail was visible. This
                    # process models the producer and is excluded from wt CPU.
                    writer_seconds = max(0.1, seconds - elapsed - 0.5)
                    writer_script = '''import datetime, json, sys, time
p, msg, duration, report = sys.argv[1:]
started = time.monotonic()
cpu = time.process_time()
count = 0
while time.monotonic() - started < float(duration):
    count += 1
    record = {'type': 'event_msg', 'timestamp': datetime.datetime.now(datetime.timezone.utc).isoformat(),
              'payload': {'type': 'agent_message', 'message': f'{msg} {count}: ' + 'synthetic agent output ' * 40}}
    with open(p, 'a') as output:
        output.write(json.dumps(record) + '\\n')
    time.sleep(0.2)
with open(report, 'w') as output:
    json.dump({'cpu_seconds': time.process_time() - cpu, 'records': count}, output)
'''
                    codex_writer_started_at = time.monotonic()
                    writer_pid = subprocess.Popen([
                        sys.executable, "-c", writer_script,
                        str(codex_path), codex_message, str(writer_seconds), str(root / "fixture-writer.json")
                    ], cwd=root, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
                    codex_writer_started = True
                if scenario == "codex_output" and codex_writer_started and codex_post_warmup_seen_at is None and codex_message.encode() in captured:
                    codex_post_warmup_seen_at = time.monotonic()
                if scenario == "lifecycle" and child is not None:
                    done, child_status, child_usage = os.wait4(child.pid, os.WNOHANG)
                    if done:
                        child_elapsed["lifecycle"] += time.monotonic() - child_started
                        child_cpu["lifecycle"] += child_usage.ru_utime + child_usage.ru_stime
                        child_log.close()
                        child = None
                        if os.waitstatus_to_exitcode(child_status) != 0:
                            raise RuntimeError(f"wt lifecycle command failed; inspect {root / ('lifecycle-new.log' if lifecycle_stage == 1 else 'lifecycle-rm.log')}")
                        if lifecycle_stage == 1:
                            lifecycle_stage = 2
                        elif lifecycle_stage == 3:
                            lifecycle_stage = 4
                if writer_pid is not None:
                    done, writer_status, writer_usage = os.wait4(writer_pid.pid, os.WNOHANG)
                    if done:
                        child_cpu["fixture_writer"] = writer_usage.ru_utime + writer_usage.ru_stime
                        writer_pid = None
                        if os.waitstatus_to_exitcode(writer_status) != 0:
                            raise RuntimeError("synthetic Codex rollout writer failed")
                if not quitting and elapsed >= seconds:
                    os.write(master, b"\x1bq" if codex_picker_opened and not codex_picker_selected else b"q")
                    quitting = True
                if not quitting and elapsed > 5:
                    interval = 0.08 if scenario == "navigation" else 5
                    if scenario in ("navigation", "refresh") and now - last_key >= interval:
                        key = (b"j" if sent % 40 < 20 else b"k") if scenario == "navigation" else b"r"
                        os.write(master, key)
                        sent += 1
                        last_key = now
                done, status, usage = os.wait4(pid, os.WNOHANG)
                if done:
                    break
                if elapsed > seconds + 8:
                    raise RuntimeError("wt did not exit after q")
        if scenario == "lifecycle":
            if lifecycle_stage != 4 or (root / "worktrees" / lifecycle_slug).exists():
                raise RuntimeError("lifecycle did not complete create and removal while the TUI was running")
        if scenario == "codex_output" and codex_post_warmup_seen_at is None:
            raise RuntimeError(f"Codex output source did not publish post-warmup append; inspect {capture}")
    finally:
        if usage is None:
            try:
                os.killpg(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            _, status, usage = os.wait4(pid, 0)
        os.close(master)
        if child is not None:
            try:
                child.kill()
            except ProcessLookupError:
                pass
            os.waitpid(child.pid, 0)
            child_log.close()
        if writer_pid is not None:
            try:
                writer_pid.kill()
            except ProcessLookupError:
                pass
            os.waitpid(writer_pid.pid, 0)
        subprocess.run(["tmux", "-L", socket, "kill-server"], env=env, capture_output=True)
    elapsed = time.monotonic() - start
    result = dict(scenario=scenario, elapsed_seconds=elapsed, first_output_seconds=first_output,
                  terminal_bytes=total_bytes, input_keys=sent, exit_code=os.waitstatus_to_exitcode(status),
                  process_tree_cpu_seconds=usage.ru_utime + usage.ru_stime,
                  process_tree_cpu_percent=100 * (usage.ru_utime + usage.ru_stime) / elapsed,
                  peak_rss_bytes=usage.ru_maxrss * (1 if sys.platform == "darwin" else 1024))
    result["driver_cpu_seconds"] = max(0.0, time.process_time() - driver_cpu_start)
    result["fixture_writer_cpu_seconds"] = child_cpu["fixture_writer"]
    result["lifecycle_cli_process_tree_cpu_seconds"] = child_cpu["lifecycle"]
    result["lifecycle_cli_elapsed_seconds"] = child_elapsed["lifecycle"]
    if scenario in ("lifecycle", "codex_output"):
        result["injected_key_to_output_ms"] = key_latency_ms
        result["key_to_output_note"] = "PTY input-to-next-terminal-bytes proxy; excludes any claim about physical display timing."
    if scenario == "codex_output":
        result["fixture_writer_report"] = json.loads((root / "fixture-writer.json").read_text())
        result["post_warmup_output_visible_seconds"] = (
            codex_post_warmup_seen_at - codex_writer_started_at if codex_post_warmup_seen_at is not None and codex_writer_started_at is not None else None
        )
        result["watcher_alive_proven_by"] = codex_message
        result["tmux_version"] = run(["tmux", "-V"], env=env).decode().strip()
        result["tmux_session_id_separator"] = "tab" if "\t" in tmux_sessions else "underscore"
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
    parser.add_argument("--ui", choices=("native", "legacy"), default="native",
                        help="Picker protocol used only for the active-output scenario")
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        parser.error("provide the executable command after --")
    executable = shutil.which(command[0])
    if executable is None:
        parser.error(f"executable not found: {command[0]}")
    command[0] = str(Path(executable).resolve())
    command[1:] = [str(Path(arg).resolve()) if Path(arg).is_file() else arg for arg in command[1:]]
    scenarios = args.scenarios.split(",")
    if any(s not in ("idle", "navigation", "refresh", "lifecycle", "codex_output") for s in scenarios):
        parser.error("scenarios: idle,navigation,refresh,lifecycle,codex_output")
    root = args.output.resolve()
    main_clone, home, config = fixture(root, args.rows)
    results = [measure(root, command, main_clone, home, config, s, args.seconds, args.ui) for s in scenarios]
    (root / "results.json").write_text(json.dumps(dict(
        command=command, rows=args.rows, platform=sys.platform, scenarios=results,
        notes=["CPU includes the waited process tree; intentionally detached daemons are excluded.",
               "first_output measures terminal activity, not a verified complete first frame.",
               "Navigation latency must come from wt's input-latency instrumentation in the isolated app log.",
               "Driver, fixture-writer, and lifecycle CLI CPU are reported separately from wt's waited process-tree CPU.",
               "codex_output uses a synthetic rollout and private tmux session; post-warmup text reaching the selected output pane proves the tail source remained active.",
               "lifecycle runs native wt new/rm while the TUI remains in its PTY; key-to-output timing is a transport proxy, not physical paint latency."],
    ), indent=2) + "\n")


if __name__ == "__main__":
    main()
