#!/usr/bin/env python3
"""Exercise the native UI with real Git worktrees and a deliberately slow Git.

All state, sockets, checkouts and child processes belong to the supplied new
scratch directory. Verifies real key-to-terminal-output liveness while source
processes are blocked, external file invalidation, and clean terminal shutdown.
"""

import argparse
import codecs
import copy
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
import shlex
import sys
import signal
import sqlite3
import struct
import subprocess
import termios
import time
import unicodedata

# The list marks the cursor row with the selection background, not a glyph.
SELECTED_ROW = b"48;2;59;66;82m"
# Freshness probes file metadata once per second. Observe a full interval plus
# source debounce/scheduling margin before assigning scans to the next phase.
GIT_QUIET_WINDOW = 1.5


class AnsiScreen:
    """Visible-cell model for Ratatui draws, not a general terminal emulator.

    Supports the cursor, erase, style and UTF-8 sequences emitted by these
    fixtures. Ambiguous-width glyphs use one column, as in the test terminal.
    """

    def __init__(self, rows=40, columns=160):
        self.rows = rows
        self.columns = columns
        self.cells = [[" "] * columns for _ in range(rows)]
        self.row = 0
        self.column = 0
        self.saved = (0, 0)
        self.state = "text"
        self.csi = bytearray()
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")

    def feed(self, data):
        for character in self.decoder.decode(data):
            byte = ord(character)
            if self.state == "escape":
                if byte == ord("["):
                    self.state = "csi"
                    self.csi.clear()
                elif byte == ord("]"):
                    self.state = "osc"
                elif byte == ord("7"):
                    self.saved = (self.row, self.column)
                    self.state = "text"
                elif byte == ord("8"):
                    self.row, self.column = self.saved
                    self.state = "text"
                elif byte == ord("c"):
                    self.clear()
                    self.state = "text"
                else:
                    self.state = "text"
                continue
            if self.state == "osc":
                if byte == 0x07:
                    self.state = "text"
                elif byte == 0x1B:
                    self.state = "osc-escape"
                continue
            if self.state == "osc-escape":
                self.state = "text" if byte == ord("\\") else "osc"
                continue
            if self.state == "csi":
                if 0x40 <= byte <= 0x7E:
                    self.apply_csi(byte)
                    self.state = "text"
                else:
                    self.csi.append(byte)
                continue
            if byte == 0x1B:
                self.state = "escape"
            elif byte == 0x0D:
                self.column = 0
            elif byte == 0x0A:
                self.row = min(self.rows - 1, self.row + 1)
            elif byte == 0x08:
                self.column = max(0, self.column - 1)
            elif byte == 0x09:
                self.column = min(self.columns - 1, (self.column // 8 + 1) * 8)
            elif byte >= 0x20 and byte != 0x7F:
                # Ratatui positions cells, not UTF-8 bytes. Combining marks
                # extend the previous cell; wide characters occupy two cells.
                if unicodedata.category(character) in ("Mn", "Me", "Cf"):
                    previous = min(self.columns - 1, self.column - 1)
                    while previous > 0 and self.cells[self.row][previous] == "":
                        previous -= 1
                    if previous >= 0:
                        self.cells[self.row][previous] += character
                    continue
                width = 2 if unicodedata.east_asian_width(character) in ("W", "F") else 1
                if self.column + width > self.columns:
                    self.column = 0
                    self.row = min(self.rows - 1, self.row + 1)
                self.erase(self.row, self.column, self.column + width)
                self.cells[self.row][self.column] = character
                if width == 2:
                    self.cells[self.row][self.column + 1] = ""
                self.column += width

    def clear(self):
        self.cells = [[" "] * self.columns for _ in range(self.rows)]
        self.row = self.column = 0

    def erase(self, row, start, end):
        start = min(start, self.columns - 1)
        end = min(end, self.columns)
        if self.cells[row][start] == "" and start:
            start -= 1
        if end < self.columns and self.cells[row][end] == "":
            end += 1
        self.cells[row][start:end] = [" "] * (end - start)

    def apply_csi(self, final):
        raw = bytes(self.csi)
        params = raw.lstrip(b"?").split(b";")

        def number(index, default=1):
            if index >= len(params) or not params[index]:
                return default
            try:
                return int(params[index])
            except ValueError:
                return default

        if final in (ord("H"), ord("f")):
            self.row = max(0, min(self.rows - 1, number(0) - 1))
            self.column = max(0, min(self.columns - 1, number(1) - 1))
        elif final == ord("A"):
            self.row = max(0, self.row - number(0))
        elif final == ord("B"):
            self.row = min(self.rows - 1, self.row + number(0))
        elif final == ord("C"):
            self.column = min(self.columns - 1, self.column + number(0))
        elif final == ord("D"):
            self.column = max(0, self.column - number(0))
        elif final == ord("J"):
            mode = number(0, 0)
            if mode == 2:
                self.cells = [[" "] * self.columns for _ in range(self.rows)]
            elif mode == 0:
                self.erase(self.row, self.column, self.columns)
                for row in range(self.row + 1, self.rows):
                    self.erase(row, 0, self.columns)
            elif mode == 1:
                for row in range(self.row):
                    self.erase(row, 0, self.columns)
                self.erase(self.row, 0, self.column + 1)
            # J3 clears scrollback, which this visible-screen model omits.
        elif final == ord("K"):
            mode = number(0, 0)
            start = 0 if mode in (1, 2) else self.column
            end = self.columns if mode in (0, 2) else self.column + 1
            self.erase(self.row, start, end)
        elif final == ord("s"):
            self.saved = (self.row, self.column)
        elif final == ord("u"):
            self.row, self.column = self.saved
        elif final == ord("h") and raw.startswith(b"?") and b"1049" in raw:
            self.clear()

    def text(self):
        return "\n".join("".join(row) for row in self.cells).encode()

    def contains(self, phrase):
        expected = re.findall(rb"[a-z0-9]+", phrase.encode().lower())
        for row in self.text().lower().splitlines():
            words = re.findall(rb"[a-z0-9]+", row)
            if any(words[index:index + len(expected)] == expected
                   for index in range(max(0, len(words) - len(expected) + 1))):
                return True
        return False


def assert_idle_frames(screen, output):
    """A half-second idle sample may cross either fixture age's second boundary.

    Filesystem birth times cannot be backdated portably. Permit each age to
    advance once, but reject all other updates, including redundant redraws.
    Ratatui finishes each differential frame by hiding the cursor.
    """
    clock = re.compile(r"\b(committed|created) ([0-9]+)([sm])\b")

    def snapshot():
        text = screen.text().decode()
        ages = {label: (int(age), unit) for label, age, unit in clock.findall(text)}
        # A digit-width transition shifts only the detail line's right padding.
        stable = "\n".join(re.sub(r" +(?=│ *$)", "", clock.sub(r"\1 <age>", line))
                           if clock.search(line) else line for line in text.splitlines())
        return stable, ages

    previous, ages = snapshot()
    advanced = set()
    marker = b"\x1b[?25l"
    frames = output.split(marker)
    assert not frames[-1], "idle terminal emitted an incomplete frame"
    for frame in frames[:-1]:
        screen.feed(frame + marker)
        current, updated = snapshot()
        assert current == previous and updated.keys() == ages.keys(), "idle terminal changed non-clock content"
        changes = {label for label in ages if ages[label] != updated[label]}
        assert changes, "idle terminal emitted a redundant frame"
        assert not changes & advanced, "idle terminal redrew a clock repeatedly"
        assert all(updated[label] == (ages[label][0] + 1, ages[label][1])
                   or (ages[label], updated[label]) == ((59, "s"), (1, "m"))
                   for label in changes), "idle terminal clock jumped"
        advanced.update(changes)
        ages = updated
    return len(frames) - 1


def startup_metadata_sample_seen(paths):
    """The first watcher fingerprint deliberately invalidates startup Git.

    Visible rows and a quiet terminal can precede that scheduled first sample.
    Require its actual observation before starting the source-quiescence wait.
    """
    for path in paths:
        for line in path.read_text(errors="replace").splitlines():
            try:
                record = json.loads(line)
            except ValueError:
                continue  # The writer may still be appending its last record.
            fields = record.get("fields", {})
            if (record.get("target") == "wt::freshness"
                    and fields.get("message") == "filesystem metadata invalidation"
                    and fields.get("git_changed") is True):
                return True
    return False


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--exit-signal", choices=["quit", "TERM", "HUP", "INT"], default="quit")
    parser.add_argument("--sections", action="store_true", help="also exercise filing and renaming through the terminal")
    parser.add_argument("--feeds", action="store_true", help="also verify log backfill, seen state and searchable help")
    parser.add_argument("--create", action="store_true", help="verify immediate creation feedback and background setup")
    parser.add_argument("--removal", action="store_true", help="verify overlapping removal menus and quit while a durable removal is held")
    args = parser.parse_args()
    if args.removal and (args.create or args.sections or args.exit_signal != "quit"):
        parser.error("--removal uses its own quit scenario; do not combine with --create, --sections or --exit-signal")
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
    install_started = root / "install-started"
    install_finished = root / "install-finished"
    if args.create:
        command = f"touch {install_started}; sleep 8; touch {install_finished}"
        config.write_text(config.read_text() + "[lifecycle]\ninstall_command = " + json.dumps(command) + "\n")
    removal_started = root / "removal-started"
    removal_release = root / "removal-release"
    removal_locks = root / "removal-locks"
    if args.removal:
        hook = root / "held-removal.py"
        hook.write_text(
            "from pathlib import Path\nimport time\n"
            f"started = Path({str(removal_started)!r}); release = Path({str(removal_release)!r})\n"
            "started.write_text(Path.cwd().name)\n"
            "deadline = time.monotonic() + 45\n"
            "while not release.exists():\n"
            "    if time.monotonic() > deadline: raise SystemExit('owned removal fixture timed out')\n"
            "    time.sleep(.05)\n"
        )
        config.write_text(config.read_text().replace(
            "[paths]\n", "[paths]\nlock_dir = " + json.dumps(str(removal_locks)) + "\n"
        ) + "\n[lifecycle]\ndestroy_command = " + json.dumps(shlex.join([sys.executable, str(hook)])) + "\n")
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
    fetch_calls = root / "git-fetch-calls"
    real_git = shutil.which("git")
    shim = tools / "git"
    shim.write_text("#!/usr/bin/env python3\nimport os, sys, time\nfrom pathlib import Path\n"
                    f"if 'fetch' in sys.argv:\n    with open({str(fetch_calls)!r}, 'a') as log: log.write('fetch\\n')\n"
                    f"if 'status' in sys.argv:\n    with open({str(status_calls)!r}, 'a') as log: log.write('status\\n')\n"
                    f"if 'status' in sys.argv and Path({str(delay)!r}).exists():\n"
                    f"    Path({str(started)!r}).touch()\n    time.sleep(2)\n"
                    f"if 'worktree' in sys.argv and 'list' in sys.argv and Path({str(delay_inventory)!r}).exists():\n"
                    f"    time.sleep(2)\n"
                    f"os.execv({real_git!r}, [{real_git!r}, *sys.argv[1:]])\n")
    shim.chmod(0o755)
    env = dict(os.environ, HOME=str(home), WT_CONFIG=str(config), TERM="xterm-256color",
               PATH=str(tools) + os.pathsep + os.environ["PATH"], WT_UPDATE="off", WT_SKILLS="off",
               WT_LOG="info,wt::freshness=debug",
               # A manual refresh queues origin work behind its ten-second floor.
               # That fetch legitimately invalidates Git, even if it completes
               # during the later title-only assertion. This fixture measures
               # local source isolation; origin maintenance has its own checks.
               WT_FETCH_ORIGIN="off", WT_GITHUB="off", WT_AUTOMATIONS="off", WT_TMUX_SOCKET=f"wt-native-qa-{os.getpid()}")
    for key in ("WT_REPO_CONFIG", "WT_REPO_ID", "WT_AGENT", "BUN_INSPECT", "XDG_CONFIG_HOME",
                "XDG_CACHE_HOME", "XDG_STATE_HOME", "NO_COLOR"):
        env.pop(key, None)
    pid, master = pty.fork()
    if pid == 0:
        os.chdir(main_clone)
        os.execve(binary, [str(binary)], env)
    fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 160, 0, 0))
    os.set_blocking(master, False)
    capture = bytearray()
    screen = AnsiScreen()
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
            screen.feed(data)
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

    def wait_for_terminal_quiet(timeout=10):
        # The first visible titles do not mean every startup source is done.
        # Let the refresh wave finish its bounded drain before measuring idle.
        # A timer that keeps repainting at 10 Hz never meets this condition
        # and fails within the timeout; the bounded clock-only check follows.
        previous = len(capture)
        unchanged_since = time.monotonic()

        def quiet(_):
            nonlocal previous, unchanged_since
            current = len(capture)
            if current != previous:
                previous = current
                unchanged_since = time.monotonic()
            return time.monotonic() - unchanged_since >= 0.5

        wait_for(quiet, timeout=timeout)

    def status_call_count():
        return status_calls.read_text().count("status\n")

    def wait_for_git_quiet(timeout=10):
        previous = status_call_count()
        unchanged_since = time.monotonic()

        def quiet(_):
            nonlocal previous, unchanged_since
            current = status_call_count()
            if current != previous:
                previous = current
                unchanged_since = time.monotonic()
            return time.monotonic() - unchanged_since >= GIT_QUIET_WINDOW

        wait_for(quiet, timeout=timeout)
        return previous

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
        # Watch registration and its initial metadata fingerprint are separate
        # startup events. The latter may legitimately repaint after half a
        # second of silence. Observe it, then require a full Git quiet window
        # and completed terminal draws before measuring idle behavior.
        wait_for(lambda _: startup_metadata_sample_seen(app_logs.glob("wt-native.*.log")), timeout=10)
        wait_for_git_quiet()
        wait_for_terminal_quiet()
        before = len(capture)
        idle_screen = copy.deepcopy(screen)
        drain_for(0.5)
        idle_clock_frames = assert_idle_frames(idle_screen, bytes(capture[before:]))
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
        # Palette preparation reads inventory off-thread. A complete shortcut
        # typed before that read finishes must stay with its original menu,
        # rather than running the board's second-key command and losing the
        # pending menu. Custom prompt is read-only until text is submitted.
        delay_inventory.touch()
        os.write(master, b"!c")
        wait_for(lambda data: b"Prompt" in data, timeout=10)
        delay_inventory.unlink()
        os.write(master, b"\x1b")
        drain_for(0.2)
        delay.touch()
        os.write(master, b"r")
        wait_for(lambda _: started.exists())
        drain_for(0.06)
        injected = time.monotonic()
        os.write(master, b"j")
        wait_for(lambda data: SELECTED_ROW in data, timeout=0.5)
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
        # Native notifications may scan immediately while the one-second
        # metadata fallback sees this same file edit later. Wait for a complete
        # quiet interval, bounded above, so that scan cannot be attributed to
        # the following title-only write. Continuous rescans still fail here.
        scans_before_title = wait_for_git_quiet()
        title_started = time.time()
        # Editing a title is a real controller command and durable write. Check
        # the resulting store, rather than mistaking input echo for success.
        os.write(master, b"t\x15Native UI title\r")
        wait_for(lambda data: b"Title saved" in data)
        with sqlite3.connect(f"file:{root / 'state/wt.sqlite'}?mode=ro", uri=True) as state:
            records = [json.loads(row[0]) for row in state.execute("SELECT data FROM repository_state")]
        assert any(record.get("slugs", {}).get("bench-001", {}).get("manualTitle") == "Native UI title"
                   for record in records), "title was not persisted on the selected worktree"
        # Include at least one metadata fallback tick after the SQLite write;
        # do not wait for quiet here, which would hide an unwanted Git refresh.
        drain_for(GIT_QUIET_WINDOW)
        if status_call_count() != scans_before_title:
            # Preserve the strict assertion while making an intermittent
            # watcher race diagnosable in CI. These are isolated fixture logs.
            events = []
            for path in sorted(app_logs.glob("wt-native.*.log"))[-4:]:
                with path.open("rb") as log:
                    log.seek(max(0, path.stat().st_size - 65536))
                    for line in log:
                        try:
                            record = json.loads(line)
                            timestamp = datetime.fromisoformat(record["timestamp"]).timestamp()
                        except (ValueError, KeyError, TypeError):
                            continue
                        if record.get("target", "").endswith("::freshness") and timestamp >= title_started - 3:
                            events.append(record)
            print(json.dumps(dict(title_git_scans_before=scans_before_title,
                                  title_git_scans_after=status_call_count(),
                                  recent_freshness_events=events[-30:])), flush=True)
        assert not fetch_calls.exists(), "background origin fetch escaped the local UI fixture isolation"
        assert status_call_count() == scans_before_title, "a title-only edit rescanned Git worktrees"
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
            wait_for(lambda data: b"removed (" in data)
            os.write(master, b"P")
            wait_for(lambda data: "perf ·".encode() in data)
            os.write(master, b"P")
            # Closing the overlay repaints only the cells it covered; the
            # history pane under it is unchanged, so any frame is the signal.
            wait_for(lambda data: bool(data))
            os.write(master, b"h")
            wait_for(lambda data: b"bench-001" in data or b"Native UI title" in data)
            os.write(master, b"\x12")
            wait_for(lambda data: b"Clear derived caches?" in data)
            os.write(master, b"\r")
            wait_for(lambda data: b"Caches cleared" in data)
            assert stored_slug("bench-001").get("manualTitle") == "Native UI title"
            assert stored_slug("bench-001").get("section") == "Today"
            drain_for(0.3)
        removal_menu_ms = None
        removal_quit_started = None
        if args.removal:
            def compact_terminal(data):
                return re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]|\s+", b"", data).lower()

            os.write(master, b"gd")
            first_menu = wait_for(lambda data: b"removeworktree?" in compact_terminal(data))
            (root / "first-removal-confirm.ansi").write_bytes(first_menu)
            os.write(master, b"y")
            wait_for(lambda _: removal_started.exists(), timeout=10)
            first_removed = removal_started.read_text()
            assert first_removed == "bench-000", first_removed
            drain_for(0.1)
            # Pending removal archives A in presentation. g must now select B;
            # its prompt must not wait for A's accepted teardown to finish.
            removal_menu_started = time.monotonic()
            os.write(master, b"gd")
            second_menu = wait_for(lambda data: b"removeworktree?" in compact_terminal(data)
                                   and re.search(rb"(bench-00[12])\(bench/\1\)", compact_terminal(data)), timeout=3)
            second_retained = re.search(rb"(bench-00[12])\(bench/\1\)", compact_terminal(second_menu)).group(1).decode()
            removal_menu_ms = (time.monotonic() - removal_menu_started) * 1000
            (root / "second-removal-confirm.ansi").write_bytes(second_menu)
            assert not removal_release.exists()
            assert (root / "worktrees" / first_removed).exists()
            # Toggle the same delete key to cancel. A bare ESC followed by q
            # before the terminal's escape timeout is Alt-q, not two keys.
            os.write(master, b"d")
            # Saving unrelated metadata must also complete while teardown is
            # held, rather than showing an optimistic title until it expires.
            drain_for(0.1)
            os.write(master, b"t\x15Title during removal\r")
            wait_for(lambda _: stored_slug(second_retained).get("manualTitle") == "Title during removal", timeout=3)
            assert not removal_release.exists()
            removal_quit_started = time.monotonic()
            os.write(master, b"q")
        elif args.create:
            delay_inventory.touch()
            os.write(master, b"n")
            wait_for(lambda data: b"New worktree name:" in data)
            injected_create = time.monotonic()
            os.write(master, b"created-feedback\r")
            wait_for(lambda data: b"created-feedback" in data, timeout=0.5)
            create_feedback_ms = (time.monotonic() - injected_create) * 1000
            # The submitted row is selected immediately, before Git resolves
            # its canonical identity. F12 reports its pending state instead of
            # attaching a different row or queueing an eventual attachment.
            os.write(master, b"\x1b[24~")
            # Differential rendering reuses cells from preceding frames.
            # Assert the visible screen, never a complete phrase in raw bytes.
            wait_for(lambda _: screen.contains("Worktree is still being created"))
            # A preference write is independent of checkout creation. Require
            # the durable value, not merely its optimistic header, while the
            # creation inventory read is still held.
            primary_file = root / "cache/harness.json"
            primary_before = json.loads(primary_file.read_text()).get("primary") if primary_file.exists() else "claude"
            os.write(master, b"\x1b[Z")
            wait_for(lambda _: primary_file.exists() and
                     json.loads(primary_file.read_text()).get("primary") != primary_before,
                     timeout=1.5)
            assert delay_inventory.exists()
            # Navigation remains ordinary while the creation read is blocked.
            # Its delayed plan and completion must not reclaim this selection.
            os.write(master, b"k")
            drain_for(0.1)
            delay_inventory.unlink()
            wait_for(lambda _: screen.contains("Created created-feedback"), timeout=15)
            assert (root / "worktrees/created-feedback").exists()
            wait_for(lambda _: install_started.exists())
            assert not install_finished.exists(), "creation waited for dependency setup"
            wait_for(lambda _: install_finished.exists(), timeout=12)
            os.write(master, b"t\x15Creation did not steal selection\r")
            wait_for(lambda _: any(stored_slug(f"bench-{index:03}").get("manualTitle")
                                   == "Creation did not steal selection" for index in range(3)))
            assert stored_slug("created-feedback").get("manualTitle") != "Creation did not steal selection"
            os.write(master, b"q")
        elif args.exit_signal == "quit":
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
        if args.exit_signal == "quit" and not args.create and not args.removal:
            with sqlite3.connect(f"file:{root / 'state/wt.sqlite'}?mode=ro", uri=True) as state:
                records = [json.loads(row[0]) for row in state.execute("SELECT data FROM repository_state")]
            assert any(record.get("slugs", {}).get("bench-001", {}).get("manualTitle") == "Queued title survives quit"
                       for record in records), "quitting dropped an accepted title write"
        removal_quit_ms = None
        if args.removal:
            removal_quit_ms = (time.monotonic() - removal_quit_started) * 1000
            assert removal_quit_ms < 4000, "quit waited for held durable removal"
            assert (root / "worktrees" / first_removed).exists(), "removal finished before release"
            removal_release.touch()
            deadline = time.monotonic() + 15
            while time.monotonic() < deadline:
                jobs = [json.loads(path.read_text()) for path in (removal_locks / "destroy-jobs").glob("*.json")]
                if any(job["state"] == "succeeded" for job in jobs):
                    break
                select.select([], [], [], 0.05)
            assert any(job["state"] == "succeeded" for job in jobs), jobs
            assert not (root / "worktrees" / first_removed).exists()
            assert (root / "worktrees" / second_retained).exists(), "cancelled second prompt removed its target"
        assert b"\x1b[?1049l" in capture, "alternate screen was not restored"
        result = dict(slow_git_seconds=2, injected_key_to_output_ms=latency_ms,
                      idle_no_unexpected_frames=True, idle_clock_frames=idle_clock_frames,
                      external_edit_refreshed=True, title_persisted=True,
                      title_edit_git_scans=0, background_origin_fetches=0,
                      section_controls=args.sections,
                      history_perf_and_hard_refresh=args.sections,
                      feed_backfill_append_and_seen=args.feeds,
                      searchable_help=args.feeds,
                      delayed_palette_chord=True,
                      accepted_write_survived_quit=args.exit_signal == "quit" and not args.create and not args.removal,
                      overlapping_removal_menu_ms=removal_menu_ms,
                      held_removal_quit_ms=removal_quit_ms,
                      durable_removal_completed_after_exit=args.removal,
                      create_feedback_ms=create_feedback_ms if args.create else None,
                      background_setup=args.create,
                      clean_shutdown=True, exit_signal=args.exit_signal)
        (root / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        print(json.dumps(result))
    finally:
        if args.removal:
            removal_release.touch()
        if args.create:
            subprocess.run(["tmux", "-L", env["WT_TMUX_SOCKET"], "kill-server"], env=env, capture_output=True)
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
