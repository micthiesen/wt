#!/usr/bin/env python3
"""Exercise the native UI with real Git worktrees and a deliberately slow Git.

All state, sockets, checkouts and child processes belong to the supplied new
scratch directory. Verifies real key-to-terminal-output liveness while source
processes are blocked, external file invalidation, and clean terminal shutdown.
"""

import argparse
from datetime import datetime, timezone
import errno
import fcntl
import importlib.util
import json
import os
from pathlib import Path
import pty
import re
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
    parser.add_argument("--feeds", action="store_true", help="also verify log backfill, seen state and searchable help")
    args = parser.parse_args()
    binary = args.binary.resolve()
    root = args.output.resolve()
    spec = importlib.util.spec_from_file_location("wt_perf", Path(__file__).with_name("perf-baseline.py"))
    fixture = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(fixture)
    main_clone, home, config = fixture.fixture(root, 3)
    app_logs = root / "logs/app"
    app_logs.mkdir(parents=True)
    config.write_text(config.read_text().replace(
        "[paths]\n", "[paths]\n" + f"log_dir = {json.dumps(str(app_logs.parent))}\n"
    ))
    feed_log = app_logs / "wt-native.fixture.log"

    def append_feed(text, channel="activity"):
        record = dict(timestamp=datetime.now(timezone.utc).isoformat(), level="INFO",
                      target="fixture", fields=dict(message=text, event_channel=channel))
        with feed_log.open("a") as stream:
            stream.write(json.dumps(record) + "\n")

    if args.feeds:
        # Use contiguous glyphs: Ratatui may skip blank cells with cursor moves.
        append_feed("Retained-attention-fixture", "attention")
        append_feed("Retained-activity-fixture")
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
        # Ratatui patches just the changed digit when an empty board was
        # painted first; raw terminal bytes need not contain "3 worktrees".
        # Unnamed rows use their first commit's subject after native Git
        # presentation resolves; the slug remains in selected details.
        wait_for(lambda data: all(title in re.sub(
            rb"\x1b\[[0-?]*[ -/]*[@-~]|\s+", b"", data
        ) for title in (b"Fixturework0", b"Fixturework1", b"Fixturework2")))
        drain_for(1)
        before = len(capture)
        drain_for(0.5)
        assert len(capture) == before, "idle terminal kept emitting frames"
        if args.feeds:
            wait_for(lambda _: b"Retained-attention-fixture" in capture)
            os.write(master, b'"')
            wait_for(lambda data: b"Retained-activity-fixture" in data)
            append_feed("Live-log-fixture")
            wait_for(lambda data: b"Live-log-fixture" in data)
            os.write(master, b'"x')

            def seen_persisted():
                with sqlite3.connect(f"file:{root / 'state/wt.sqlite'}?mode=ro", uri=True) as state:
                    return any(json.loads(row[0]).get("attentionSeenTs", 0) > 0
                               for row in state.execute("SELECT data FROM repository_state"))

            wait_for(lambda _: seen_persisted())
            os.write(master, b"?/merge")
            wait_for(lambda data: b"Togglemergewhenready" in re.sub(
                rb"\x1b\[[0-?]*[ -/]*[@-~]|\s+", b"", data
            ))
            # First Esc clears the search, second closes help.
            os.write(master, b"\x1b")
            drain_for(0.1)
            os.write(master, b"\x1b")
            drain_for(0.2)
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
        scans_before_edit = status_calls.read_text().count("status\n")
        (root / "worktrees/bench-001/example.txt").write_text("external edit\n")
        # A fast refresh can settle before the renderer observes Refreshing;
        # require the watcher-driven scan and a changed terminal frame instead.
        wait_for(lambda data: status_calls.read_text().count("status\n") > scans_before_edit and bool(data))
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
            os.write(master, b"h")
            wait_for(lambda data: b"Removed" in data)
            os.write(master, b"P")
            wait_for(lambda data: b"Performance" in data)
            os.write(master, b"P")
            # The overlay leaves the history header visible, so closing it
            # repaints the body rather than emitting the header again.
            wait_for(lambda data: b"No recently removed worktrees" in data)
            os.write(master, b"h")
            wait_for(lambda data: b"bench-001" in data or b"Native UI title" in data)
            os.write(master, b"\x12")
            wait_for(lambda data: b"Clear derived caches?" in data)
            os.write(master, b"\r")
            wait_for(lambda data: b"Caches cleared" in data)
            assert stored_slug("bench-001").get("manualTitle") == "Native UI title"
            assert stored_slug("bench-001").get("section") == "Today"
            drain_for(0.3)
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
                      history_perf_and_hard_refresh=args.sections,
                      feed_backfill_append_and_seen=args.feeds,
                      searchable_help=args.feeds,
                      accepted_write_survived_quit=args.exit_signal == "quit",
                      clean_shutdown=True, exit_signal=args.exit_signal)
        (root / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        print(json.dumps(result))
    finally:
        (root / "terminal.ansi").write_bytes(capture)
        # Close the PTY before waiting: Darwin can leave a child exiting while
        # the still-open master holds undrained terminal output.
        os.close(master)
        if not reaped:
            done, _ = os.waitpid(pid, os.WNOHANG)
            if not done:
                try:
                    os.kill(pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline:
                    done, _ = os.waitpid(pid, os.WNOHANG)
                    if done:
                        break
                    select.select([], [], [], 0.05)
                if not done:
                    os.kill(pid, signal.SIGKILL)
                    os.waitpid(pid, 0)


if __name__ == "__main__":
    main()
