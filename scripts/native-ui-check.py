#!/usr/bin/env python3
"""Exercise the native UI with real Git worktrees and a deliberately slow Git.

All state, sockets, checkouts and child processes belong to the supplied new
scratch directory. Verifies real key-to-terminal-output liveness while source
processes are blocked, external file invalidation, and clean terminal shutdown.
"""

import argparse
import errno
import fcntl
import importlib.util
import json
import os
from pathlib import Path
import pty
import select
import shutil
import signal
import sqlite3
import struct
import termios
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--exit-signal", choices=["quit", "TERM", "HUP", "INT"], default="quit")
    parser.add_argument("--sections", action="store_true", help="also exercise filing and renaming through the terminal")
    args = parser.parse_args()
    binary = args.binary.resolve()
    root = args.output.resolve()
    spec = importlib.util.spec_from_file_location("wt_perf", Path(__file__).with_name("perf-baseline.py"))
    fixture = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(fixture)
    main_clone, home, config = fixture.fixture(root, 3)
    tools = root / "tools"
    tools.mkdir()
    delay = root / "delay-git"
    delay_inventory = root / "delay-inventory"
    started = root / "git-delayed"
    status_calls = root / "git-status-calls"
    real_git = shutil.which("git")
    shim = tools / "git"
    shim.write_text("#!/usr/bin/env python3\nimport os, sys, time\nfrom pathlib import Path\n"
                    f"if 'status' in sys.argv:\n    with open({str(status_calls)!r}, 'a') as log: log.write('status\\n')\n"
                    f"if 'status' in sys.argv and Path({str(delay)!r}).exists():\n"
                    f"    Path({str(started)!r}).touch()\n    time.sleep(2)\n"
                    f"if 'worktree' in sys.argv and 'list' in sys.argv and Path({str(delay_inventory)!r}).exists():\n"
                    f"    time.sleep(2)\n"
                    f"os.execv({real_git!r}, [{real_git!r}, *sys.argv[1:]])\n")
    shim.chmod(0o755)
    env = dict(os.environ, HOME=str(home), WT_CONFIG=str(config), TERM="xterm-256color",
               PATH=str(tools) + os.pathsep + os.environ["PATH"], WT_UPDATE="off", WT_SKILLS="off",
               WT_GITHUB="off", WT_AUTOMATIONS="off", WT_TMUX_SOCKET=f"wt-native-qa-{os.getpid()}")
    for key in ("WT_REPO_CONFIG", "WT_REPO_ID", "WT_AGENT", "BUN_INSPECT", "XDG_CONFIG_HOME",
                "XDG_CACHE_HOME", "XDG_STATE_HOME"):
        env.pop(key, None)
    pid, master = pty.fork()
    if pid == 0:
        os.chdir(main_clone)
        os.execve(binary, [str(binary)], env)
    fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 160, 0, 0))
    os.set_blocking(master, False)
    capture = bytearray()
    reaped = False

    def read(timeout=0.01):
        if select.select([master], [], [], timeout)[0]:
            try:
                data = os.read(master, 65536)
            except OSError as error:
                if error.errno == errno.EIO:
                    return b""
                raise
            capture.extend(data)
            return data
        return b""

    def wait_for(predicate, timeout=5):
        deadline = time.monotonic() + timeout
        output = bytearray()
        while time.monotonic() < deadline:
            output.extend(read())
            if predicate(bytes(output)):
                return bytes(output)
        raise AssertionError(f"terminal condition timed out; inspect {root / 'terminal.ansi'}")

    def drain_for(seconds):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            read()

    def stored_slug(slug):
        with sqlite3.connect(f"file:{root / 'state/wt.sqlite'}?mode=ro", uri=True) as state:
            records = [json.loads(row[0]) for row in state.execute("SELECT data FROM repository_state")]
        return next((record.get("slugs", {}).get(slug, {}) for record in records
                     if slug in record.get("slugs", {})), {})

    try:
        wait_for(lambda data: b"3 worktrees" in data)
        drain_for(1)
        before = len(capture)
        drain_for(0.5)
        assert len(capture) == before, "idle terminal kept emitting frames"
        delay.touch()
        os.write(master, b"r")
        wait_for(lambda _: started.exists())
        drain_for(0.06)
        injected = time.monotonic()
        os.write(master, b"j")
        wait_for(lambda data: "›".encode() in data, timeout=0.5)
        latency_ms = (time.monotonic() - injected) * 1000
        # The source still has blocked Git processes, so this frame proves the
        # input owner was not waiting for their completion.
        assert latency_ms < 500
        delay.unlink()
        drain_for(3)
        (root / "worktrees/bench-001/example.txt").write_text("external edit\n")
        wait_for(lambda data: b"refreshing" in data)
        drain_for(0.5)
        # Editing a title is a real controller command and durable write. Check
        # the resulting store, rather than mistaking input echo for success.
        scans_before_title = status_calls.read_text().count("status\n")
        os.write(master, b"t\x15Native UI title\r")
        wait_for(lambda data: b"Title saved" in data)
        with sqlite3.connect(f"file:{root / 'state/wt.sqlite'}?mode=ro", uri=True) as state:
            records = [json.loads(row[0]) for row in state.execute("SELECT data FROM repository_state")]
        assert any(record.get("slugs", {}).get("bench-001", {}).get("manualTitle") == "Native UI title"
                   for record in records), "title was not persisted on the selected worktree"
        drain_for(0.5)
        assert status_calls.read_text().count("status\n") == scans_before_title, "a title-only edit rescanned Git worktrees"
        if args.sections:
            os.write(master, b"l")
            wait_for(lambda data: b"Move bench-001 to section" in data)
            os.write(master, b"nRelease\r")
            wait_for(lambda _: stored_slug("bench-001").get("section") == "Release")
            drain_for(0.2)
            # Filing holds the cursor's place, so the next edit belongs to the
            # surviving neighbor, not the row now in another section.
            os.write(master, b"t\x15Neighbor stayed\r")
            wait_for(lambda _: stored_slug("bench-002").get("manualTitle") == "Neighbor stayed")
            with sqlite3.connect(f"file:{root / 'state/wt.sqlite'}?mode=ro", uri=True) as state:
                records = [json.loads(row[0]) for row in state.execute("SELECT data FROM repository_state")]
            assert any(record.get("slugs", {}).get("bench-002", {}).get("manualTitle") == "Neighbor stayed"
                       and record.get("slugs", {}).get("bench-001", {}).get("section") == "Release"
                       for record in records), "filing moved the cursor away from its neighbor"
            # Ctrl+D enters the next expanded section at its first row.
            os.write(master, b"\x04L\x15Today\r")
            wait_for(lambda _: stored_slug("bench-001").get("section") == "Today")
            drain_for(0.2)
            with sqlite3.connect(f"file:{root / 'state/wt.sqlite'}?mode=ro", uri=True) as state:
                records = [json.loads(row[0]) for row in state.execute("SELECT data FROM repository_state")]
            assert any(record.get("slugs", {}).get("bench-001", {}).get("section") == "Today"
                       for record in records), "renaming did not preserve section membership"
        if args.exit_signal == "quit":
            # The action has been admitted but its inventory read has not
            # completed. Quitting must drain that action, not drop its write.
            delay_inventory.touch()
            os.write(master, b"t\x15Queued title survives quit\rq")
        else:
            started.unlink(missing_ok=True)
            delay.touch()
            os.write(master, b"r")
            wait_for(lambda _: started.exists())
            os.kill(pid, getattr(signal, "SIG" + args.exit_signal))
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            read()
            done, status = os.waitpid(pid, os.WNOHANG)
            if done:
                reaped = True
                assert os.waitstatus_to_exitcode(status) == 0
                break
        assert reaped, "native terminal did not shut down"
        if args.exit_signal == "quit":
            with sqlite3.connect(f"file:{root / 'state/wt.sqlite'}?mode=ro", uri=True) as state:
                records = [json.loads(row[0]) for row in state.execute("SELECT data FROM repository_state")]
            assert any(record.get("slugs", {}).get("bench-001", {}).get("manualTitle") == "Queued title survives quit"
                       for record in records), "quitting dropped an accepted title write"
        assert b"\x1b[?1049l" in capture, "alternate screen was not restored"
        result = dict(slow_git_seconds=2, injected_key_to_output_ms=latency_ms,
                      idle_no_frames=True, external_edit_refreshed=True, title_persisted=True,
                      title_edit_git_scans=0,
                      section_controls=args.sections,
                      accepted_write_survived_quit=args.exit_signal == "quit",
                      clean_shutdown=True, exit_signal=args.exit_signal)
        (root / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        print(json.dumps(result))
    finally:
        (root / "terminal.ansi").write_bytes(capture)
        if not reaped:
            try:
                os.killpg(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            os.waitpid(pid, 0)
        os.close(master)


if __name__ == "__main__":
    main()
